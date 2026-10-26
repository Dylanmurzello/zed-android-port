//! The SIGSYS trap handler and the seccomp filter that feeds it.
//!
//! The kernel delivers a tagged SIGSYS for every path syscall the guest
//! issues; the handler translates the path argument, re-issues the
//! syscall from loader text (which the filter allows by instruction
//! pointer) and writes the result into the saved `x0`. Untagged SIGSYS
//! come from Android's own per-app filter and are answered from the
//! compat table (`ENOSYS`, `EPERM`, or success under fake root).

use core::mem::offset_of;
use core::sync::atomic::{AtomicU64, Ordering};

use zd_engine_core::bpf::{self, FilterSpec, SockFilter};
use zd_engine_core::compat::{self, Response};
use zd_engine_core::path::{MAX_PATH, Writer, is_absolute};
use zd_engine_core::sysno::{
    MAX_TRAPPED, PROBE_ACK, PROBE_MAGIC, TAG_PATH, TAG_PROBE, TRAP_TABLE, TRAP_TABLE_FAKE_ROOT,
    is_engine_tag, nr,
};
use zd_engine_core::translate::{ProcExe, Translated, Translator};
use zd_engine_core::{LOADER_IMAGE_BASE, LOADER_WINDOW_SIZE};

use crate::config;
use crate::exec;
use crate::log;
use crate::sys::{self, KernelSigaction};

/// `siginfo_t` for `SYS_SECCOMP`: the kernel's `_sigsys` union member.
#[repr(C)]
pub struct SigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    _pad: i32,
    pub si_call_addr: u64,
    pub si_syscall: i32,
    pub si_arch: u32,
    _rest: [u8; 96],
}

/// arm64 `struct sigcontext`.
#[repr(C, align(16))]
pub struct SigContext {
    pub fault_address: u64,
    pub regs: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
    _reserved: [u8; 4096],
}

/// arm64 `struct ucontext`.
#[repr(C)]
pub struct UContext {
    pub uc_flags: u64,
    pub uc_link: u64,
    pub uc_stack: [u64; 3],
    pub uc_sigmask: u64,
    _unused: [u8; 120],
    pub uc_mcontext: SigContext,
}

const _: () = {
    assert!(offset_of!(SigInfo, si_errno) == 4);
    assert!(offset_of!(SigInfo, si_call_addr) == 16);
    assert!(offset_of!(SigInfo, si_syscall) == 24);
    assert!(size_of::<SigInfo>() == 128);
    assert!(offset_of!(UContext, uc_mcontext) == 176);
    assert!(offset_of!(UContext, uc_mcontext) + offset_of!(SigContext, regs) == 184);
    assert!(offset_of!(UContext, uc_mcontext) + offset_of!(SigContext, sp) == 432);
    assert!(offset_of!(UContext, uc_mcontext) + offset_of!(SigContext, pc) == 440);
};

const SYS_SECCOMP: i32 = 1;

pub static TRAP_COUNT: AtomicU64 = AtomicU64::new(0);
pub static COMPAT_COUNT: AtomicU64 = AtomicU64::new(0);
static COMPAT_SEEN: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// The SIGSYS disposition the guest asked for. The real handler stays
/// ours; this record is what the guest reads back.
static GUEST_SIGSYS: config::Global<KernelSigaction> = config::Global::new(KernelSigaction {
    handler: 0,
    flags: 0,
    restorer: 0,
    mask: 0,
});

pub fn install() -> Result<(), i32> {
    let action = KernelSigaction {
        handler: (sigsys as extern "C" fn(i32, *mut SigInfo, *mut UContext)) as usize as u64,
        // SA_NODEFER is load-bearing: a trap taken while SIGSYS is
        // blocked makes the kernel reset the disposition and kill the
        // process, and the signal would be blocked inside the handler
        // otherwise.
        flags: sys::SA_SIGINFO | sys::SA_NODEFER | sys::SA_RESTART,
        restorer: 0,
        mask: 0,
    };
    sys::rt_sigaction(sys::SIGSYS, &action)
}

/// Clear SIGSYS from the inherited mask for the same reason.
pub fn unblock_sigsys() -> Result<(), i32> {
    sys::rt_sigprocmask(sys::SIG_UNBLOCK, &sys::SIGSYS_BIT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterState {
    /// A parent process already installed the engine filter; filters
    /// inherit across fork and exec.
    Inherited,
    Installed {
        tsync: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct FilterError {
    pub stage: &'static str,
    pub errno: i32,
}

pub fn probe() -> bool {
    sys::getpid_probe(PROBE_MAGIC) == PROBE_ACK
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

pub fn ensure_filter() -> Result<FilterState, FilterError> {
    if probe() {
        return Ok(FilterState::Inherited);
    }
    let no_new_privs = sys::prctl(sys::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0).map_err(|errno| FilterError {
        stage: "PR_GET_NO_NEW_PRIVS",
        errno,
    })?;
    if no_new_privs == 0 {
        sys::prctl(sys::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0).map_err(|errno| FilterError {
            stage: "PR_SET_NO_NEW_PRIVS",
            errno,
        })?;
    }
    let mut trapped = [0u32; MAX_TRAPPED];
    let mut count = 0usize;
    for &number in TRAP_TABLE {
        trapped[count] = number;
        count += 1;
    }
    if config::get().fake_root {
        for &number in TRAP_TABLE_FAKE_ROOT {
            trapped[count] = number;
            count += 1;
        }
    }
    let program = bpf::build(&FilterSpec {
        trapped: &trapped[..count],
        window_start: LOADER_IMAGE_BASE,
        window_end: LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE,
        probe_nr: nr::GETPID,
        probe_magic: PROBE_MAGIC,
        path_tag: TAG_PATH,
        probe_tag: TAG_PROBE,
    })
    .map_err(|_| FilterError {
        stage: "bpf build",
        errno: sys::EINVAL,
    })?;
    let fprog = SockFprog {
        len: program.len() as u16,
        filter: program.insns().as_ptr(),
    };
    let tsync = match sys::seccomp(
        sys::SECCOMP_SET_MODE_FILTER,
        sys::SECCOMP_FILTER_FLAG_TSYNC,
        &fprog as *const SockFprog as usize,
    ) {
        Ok(_) => true,
        Err(_) => {
            sys::prctl(
                sys::PR_SET_SECCOMP,
                sys::SECCOMP_MODE_FILTER,
                &fprog as *const SockFprog as usize,
                0,
                0,
            )
            .map_err(|errno| FilterError {
                stage: "PR_SET_SECCOMP",
                errno,
            })?;
            false
        }
    };
    if !probe() {
        return Err(FilterError {
            stage: "probe after install",
            errno: 0,
        });
    }
    Ok(FilterState::Installed { tsync })
}

pub extern "C" fn sigsys(_signal: i32, info: *mut SigInfo, context: *mut UContext) {
    // SAFETY: the kernel hands valid frames to an SA_SIGINFO handler.
    let (info, context) = unsafe { (&*info, &mut *context) };
    if info.si_code != SYS_SECCOMP {
        return;
    }
    TRAP_COUNT.fetch_add(1, Ordering::Relaxed);
    let tag = info.si_errno as u16;
    let number = info.si_syscall as u32;
    let regs = &mut context.uc_mcontext.regs;
    if tag == TAG_PROBE {
        regs[0] = PROBE_ACK;
        return;
    }
    if !is_engine_tag(tag) {
        compat_trap(number, regs);
        return;
    }
    dispatch(number, regs);
}

/// Android's filter trapped a syscall bionic never makes.
fn compat_trap(number: u32, regs: &mut [u64; 31]) {
    COMPAT_COUNT.fetch_add(1, Ordering::Relaxed);
    regs[0] = match compat::response(number, config::get().fake_root) {
        Response::Errno(errno) => sys::neg_errno(errno),
        Response::Success => 0,
        Response::Chroot => chroot(regs),
        Response::Unknown => {
            log_unknown(number);
            sys::neg_errno(sys::ENOSYS)
        }
    };
}

fn log_unknown(number: u32) {
    let word = (number / 64) as usize;
    if word >= COMPAT_SEEN.len() {
        return;
    }
    let bit = 1u64 << (number % 64);
    if COMPAT_SEEN[word].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        let mut line = log::Line::new();
        line.push(b"zdload: android filter trapped syscall ");
        line.push_u64(u64::from(number));
        line.push(b"; returned ENOSYS\n");
        log::append_to_log_file(line.as_bytes());
    }
}

/// `chroot` succeeds only when the target is the guest root itself,
/// which is what `debootstrap`-style tools ask for.
fn chroot(regs: &[u64; 31]) -> u64 {
    let Some(translator) = config::translator() else {
        return sys::neg_errno(sys::EPERM);
    };
    let Some(guest) = (unsafe { cstr_bounded(regs[0] as *const u8, MAX_PATH) }) else {
        return sys::neg_errno(sys::ENAMETOOLONG);
    };
    let mut host = [0u8; MAX_PATH];
    match translator.guest_to_host(guest, &mut host) {
        Ok(Translated::Host(len)) if host[..len] == *translator.rootfs => 0,
        _ => sys::neg_errno(sys::EPERM),
    }
}

/// Bytes up to the NUL, or `None` when no NUL appears within `max`.
pub unsafe fn cstr_bounded<'a>(pointer: *const u8, max: usize) -> Option<&'a [u8]> {
    if pointer.is_null() {
        return None;
    }
    for len in 0..max {
        // SAFETY: the caller supplies a guest pointer that the guest
        // would have handed to the kernel; a bad one faults here instead.
        if unsafe { *pointer.add(len) } == 0 {
            return Some(unsafe { core::slice::from_raw_parts(pointer, len) });
        }
    }
    None
}

fn args_of(regs: &[u64; 31]) -> [usize; 6] {
    [
        regs[0] as usize,
        regs[1] as usize,
        regs[2] as usize,
        regs[3] as usize,
        regs[4] as usize,
        regs[5] as usize,
    ]
}

fn issue(number: u32, args: &[usize; 6]) -> u64 {
    unsafe { sys::syscall6(number, args[0], args[1], args[2], args[3], args[4], args[5]) as u64 }
}

fn is_self(who: ProcExe) -> bool {
    match who {
        ProcExe::SelfProcess | ProcExe::ThreadSelf => true,
        ProcExe::Pid(pid) => pid == sys::getpid(),
    }
}

fn dispatch(number: u32, regs: &mut [u64; 31]) {
    let Some(translator) = config::translator() else {
        regs[0] = sys::neg_errno(sys::ENOSYS);
        return;
    };
    match number {
        nr::SETXATTR
        | nr::LSETXATTR
        | nr::GETXATTR
        | nr::LGETXATTR
        | nr::LISTXATTR
        | nr::LLISTXATTR
        | nr::REMOVEXATTR
        | nr::LREMOVEXATTR
        | nr::STATFS
        | nr::TRUNCATE
        | nr::CHDIR => path_syscall(translator, number, regs, 0),
        nr::INOTIFY_ADD_WATCH
        | nr::MKNODAT
        | nr::MKDIRAT
        | nr::UNLINKAT
        | nr::FACCESSAT
        | nr::FCHMODAT
        | nr::FCHOWNAT
        | nr::OPENAT
        | nr::READLINKAT
        | nr::NEWFSTATAT
        | nr::UTIMENSAT
        | nr::NAME_TO_HANDLE_AT
        | nr::STATX => path_syscall(translator, number, regs, 1),
        nr::EXECVE | nr::EXECVEAT => exec::handle(number, regs),
        nr::SYMLINKAT => symlinkat(translator, regs),
        nr::LINKAT | nr::RENAMEAT | nr::RENAMEAT2 => two_paths(translator, number, regs),
        nr::GETCWD => getcwd(translator, regs),
        nr::BIND | nr::CONNECT => sockaddr(translator, number, regs, 1, 2),
        nr::SENDTO => sockaddr(translator, number, regs, 4, 5),
        nr::RT_SIGACTION => sigaction(regs),
        nr::RT_SIGPROCMASK => sigprocmask(regs),
        nr::FSTAT => {
            regs[0] = issue(number, &args_of(regs));
            fake_owner(regs[0], regs[1] as *mut u8, false);
        }
        nr::FCHOWN => {
            let result = issue(number, &args_of(regs));
            regs[0] = if result == sys::neg_errno(sys::EPERM) { 0 } else { result };
        }
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID | nr::GETGROUPS => regs[0] = 0,
        nr::SETUID
        | nr::SETGID
        | nr::SETREUID
        | nr::SETREGID
        | nr::SETRESUID
        | nr::SETRESGID
        | nr::SETFSUID
        | nr::SETFSGID
        | nr::SETGROUPS
        | nr::CAPSET => regs[0] = 0,
        nr::GETRESUID | nr::GETRESGID => {
            for &pointer in &regs[0..3] {
                let pointer = pointer as *mut u32;
                if !pointer.is_null() {
                    // SAFETY: guest-supplied output pointers, as for the kernel.
                    unsafe { pointer.write_unaligned(0) };
                }
            }
            regs[0] = 0;
        }
        _ => regs[0] = sys::neg_errno(sys::ENOSYS),
    }
}

/// Syscalls with one path argument at `index` whose other arguments pass
/// through unchanged, plus their result fixups.
fn path_syscall(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31], index: usize) {
    let Some(guest) = (unsafe { cstr_bounded(regs[index] as *const u8, MAX_PATH) }) else {
        regs[0] = sys::neg_errno(if regs[index] == 0 { sys::EFAULT } else { sys::ENAMETOOLONG });
        return;
    };
    let mut host = [0u8; MAX_PATH];
    let mut args = args_of(regs);
    let result = match translator.guest_to_host(guest, &mut host) {
        Ok(Translated::Relative) => issue(number, &args),
        Ok(Translated::Host(_)) => {
            args[index] = host.as_ptr() as usize;
            issue(number, &args)
        }
        Ok(Translated::ProcExe(who)) if is_self(who) => {
            let guest_exe = config::get().guest_exe.as_bytes();
            if number == nr::READLINKAT {
                regs[0] = copy_out(regs[2] as *mut u8, regs[3] as usize, guest_exe);
                return;
            }
            args[index] = match translator.guest_to_host(guest_exe, &mut host) {
                Ok(Translated::Host(_)) => host.as_ptr() as usize,
                _ => config::get().guest_exe.as_ptr() as usize,
            };
            issue(number, &args)
        }
        Ok(Translated::ProcExe(_)) => issue(number, &args),
        Ok(Translated::ProcAuxv(who)) if is_self(who) && number == nr::OPENAT => serve_auxv(regs),
        Ok(Translated::ProcAuxv(_)) => issue(number, &args),
        Err(_) => sys::neg_errno(sys::ENAMETOOLONG),
    };
    regs[0] = result;
    match number {
        nr::READLINKAT => detranslate_readlink(translator, regs, &mut host),
        nr::NEWFSTATAT => fake_owner(result, regs[2] as *mut u8, false),
        nr::STATX => fake_owner(result, regs[4] as *mut u8, true),
        nr::FCHOWNAT if result == sys::neg_errno(sys::EPERM) => regs[0] = 0,
        nr::MKNODAT if result == sys::neg_errno(sys::EPERM) => {
            regs[0] = device_placeholder(args[0] as isize, args[1] as *const u8, regs[2] as u32);
        }
        _ => {}
    }
}

/// Under fake root a device node the app may not create becomes an
/// empty file, which is what package scripts that `mknod` then proceed
/// against.
fn device_placeholder(dirfd: isize, path: *const u8, mode: u32) -> u64 {
    if !config::get().fake_root {
        return sys::neg_errno(sys::EPERM);
    }
    let kind = mode & sys::S_IFMT;
    if kind != sys::S_IFCHR && kind != sys::S_IFBLK {
        return sys::neg_errno(sys::EPERM);
    }
    match sys::openat(
        dirfd,
        path,
        sys::O_WRONLY | sys::O_CREAT | sys::O_EXCL | sys::O_CLOEXEC,
        (mode & 0o777) as usize,
    ) {
        Ok(fd) => {
            if let Err(_errno) = sys::close(fd) {}
            0
        }
        Err(errno) => sys::neg_errno(errno),
    }
}

/// Present files owned by the app as root's. `struct stat` keeps uid at
/// 24 and gid at 28; `struct statx` at 20 and 24.
fn fake_owner(result: u64, buffer: *mut u8, statx: bool) {
    let config = config::get();
    if !config.fake_root || result != 0 || buffer.is_null() {
        return;
    }
    let (uid_offset, gid_offset) = if statx { (20usize, 24usize) } else { (24usize, 28usize) };
    // SAFETY: the kernel just filled this guest buffer.
    unsafe {
        let uid = buffer.add(uid_offset) as *mut u32;
        let gid = buffer.add(gid_offset) as *mut u32;
        if uid.read_unaligned() == config.real_uid {
            uid.write_unaligned(0);
        }
        if gid.read_unaligned() == config.real_gid {
            gid.write_unaligned(0);
        }
    }
}

/// `linkat`, `renameat`, `renameat2`: paths at x1 and x3.
#[inline(never)]
fn two_paths(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31]) {
    let mut args = args_of(regs);
    let mut first = [0u8; MAX_PATH];
    let mut second = [0u8; MAX_PATH];
    for (index, buffer) in [(1usize, &mut first), (3usize, &mut second)] {
        let Some(guest) = (unsafe { cstr_bounded(regs[index] as *const u8, MAX_PATH) }) else {
            regs[0] = sys::neg_errno(if regs[index] == 0 { sys::EFAULT } else { sys::ENAMETOOLONG });
            return;
        };
        match translator.guest_to_host(guest, buffer) {
            Ok(Translated::Host(_)) => args[index] = buffer.as_ptr() as usize,
            Ok(_) => {}
            Err(_) => {
                regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
                return;
            }
        }
    }
    regs[0] = issue(number, &args);
}

/// Guest absolute path of the directory `dirfd` refers to, via the
/// kernel's own view of the fd.
fn guest_dir_of_fd(translator: &Translator<'static>, dirfd: isize, out: &mut [u8; MAX_PATH]) -> Option<usize> {
    let mut host = [0u8; MAX_PATH];
    let len = if dirfd == sys::AT_FDCWD {
        sys::getcwd(&mut host).ok()?
    } else {
        let mut link = [0u8; 32];
        let mut writer = Writer::new(&mut link);
        writer.push(b"/proc/self/fd/").ok()?;
        let mut digits = [0u8; 20];
        writer.push(log::itoa(&mut digits, dirfd as u64)).ok()?;
        let len = writer.finish();
        let _ = len;
        sys::readlinkat(sys::AT_FDCWD, link.as_ptr(), &mut host).ok()?
    };
    translator.host_to_guest(&host[..len], out).ok()
}

/// `symlinkat(target, dirfd, linkpath)`: the stored target is rewritten
/// so the kernel can resolve the link from the host side.
#[inline(never)]
fn symlinkat(translator: &Translator<'static>, regs: &mut [u64; 31]) {
    let (Some(target), Some(link)) = (
        unsafe { cstr_bounded(regs[0] as *const u8, MAX_PATH) },
        unsafe { cstr_bounded(regs[2] as *const u8, MAX_PATH) },
    ) else {
        regs[0] = sys::neg_errno(sys::EFAULT);
        return;
    };
    let dirfd = regs[1] as isize;
    let mut link_guest = [0u8; MAX_PATH];
    let link_guest_len = if is_absolute(link) {
        let len = link.len().min(MAX_PATH - 1);
        link_guest[..len].copy_from_slice(&link[..len]);
        len
    } else {
        let Some(dir_len) = guest_dir_of_fd(translator, dirfd, &mut link_guest) else {
            regs[0] = sys::neg_errno(sys::EACCES);
            return;
        };
        let mut writer = Writer::new(&mut link_guest);
        if writer.push(b"").is_err() {
            regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
            return;
        }
        // Continue after the directory already in the buffer.
        let mut tail = [0u8; MAX_PATH];
        let mut tail_writer = Writer::new(&mut tail);
        if tail_writer.push(&link_guest[..dir_len]).is_err()
            || tail_writer.push_component(link).is_err()
        {
            regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
            return;
        }
        let len = tail_writer.finish();
        link_guest[..len].copy_from_slice(&tail[..len]);
        len
    };
    let mut new_target = [0u8; MAX_PATH];
    let Ok(_) = translator.rewrite_symlink_target(&link_guest[..link_guest_len], target, &mut new_target) else {
        regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
        return;
    };
    let mut host_link = [0u8; MAX_PATH];
    let mut args = args_of(regs);
    args[0] = new_target.as_ptr() as usize;
    match translator.guest_to_host(link, &mut host_link) {
        Ok(Translated::Host(_)) => args[2] = host_link.as_ptr() as usize,
        Ok(_) => {}
        Err(_) => {
            regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
            return;
        }
    }
    regs[0] = issue(nr::SYMLINKAT, &args);
}

/// `getcwd(buf, size)`: the kernel answers with the host path.
#[inline(never)]
fn getcwd(translator: &Translator<'static>, regs: &mut [u64; 31]) {
    let buffer = regs[0] as *mut u8;
    let size = regs[1] as usize;
    let mut host = [0u8; MAX_PATH];
    let len = match sys::getcwd(&mut host) {
        Ok(len) => len,
        Err(errno) => {
            regs[0] = sys::neg_errno(errno);
            return;
        }
    };
    let mut guest = [0u8; MAX_PATH];
    let Ok(guest_len) = translator.host_to_guest(&host[..len], &mut guest) else {
        regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
        return;
    };
    if buffer.is_null() {
        regs[0] = sys::neg_errno(sys::EFAULT);
        return;
    }
    if guest_len + 1 > size {
        regs[0] = sys::neg_errno(sys::ERANGE);
        return;
    }
    // SAFETY: a guest buffer of `size` bytes, as handed to the kernel.
    unsafe { core::ptr::copy_nonoverlapping(guest.as_ptr(), buffer, guest_len + 1) };
    regs[0] = (guest_len + 1) as u64;
}

/// Filesystem `sockaddr_un` paths in `bind`, `connect` and `sendto`.
#[inline(never)]
fn sockaddr(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31], addr_index: usize, len_index: usize) {
    let mut args = args_of(regs);
    let address = regs[addr_index] as *const u8;
    let len = regs[len_index] as usize;
    if address.is_null() || len < 3 {
        regs[0] = issue(number, &args);
        return;
    }
    // SAFETY: a guest sockaddr of `len` bytes, as handed to the kernel.
    let bytes = unsafe { core::slice::from_raw_parts(address, len.min(2 + sys::SUN_PATH_MAX)) };
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    let path = &bytes[2..];
    let path = match path.iter().position(|&byte| byte == 0) {
        Some(end) => &path[..end],
        None => path,
    };
    if family != sys::AF_UNIX || path.is_empty() {
        regs[0] = issue(number, &args);
        return;
    }
    let mut host = [0u8; MAX_PATH];
    let host_len = match translator.guest_to_host(path, &mut host) {
        Ok(Translated::Host(len)) => len,
        Ok(_) => {
            regs[0] = issue(number, &args);
            return;
        }
        Err(_) => {
            regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
            return;
        }
    };
    if host_len >= sys::SUN_PATH_MAX {
        regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
        return;
    }
    let mut sun = [0u8; 2 + sys::SUN_PATH_MAX];
    sun[..2].copy_from_slice(&sys::AF_UNIX.to_le_bytes());
    sun[2..2 + host_len].copy_from_slice(&host[..host_len]);
    args[addr_index] = sun.as_ptr() as usize;
    args[len_index] = 2 + host_len + 1;
    regs[0] = issue(number, &args);
}

/// `rt_sigprocmask(how, set, oldset, sigsetsize)`: SIGSYS never enters
/// the blocked set, whatever the guest asks.
fn sigprocmask(regs: &mut [u64; 31]) {
    let how = regs[0] as usize;
    let set = regs[1] as *const u64;
    let mut args = args_of(regs);
    if set.is_null() || how == sys::SIG_UNBLOCK || regs[3] != 8 {
        regs[0] = issue(nr::RT_SIGPROCMASK, &args);
        return;
    }
    // SAFETY: a guest sigset of 8 bytes, as handed to the kernel.
    let copy = unsafe { set.read_unaligned() } & !sys::SIGSYS_BIT;
    args[1] = &copy as *const u64 as usize;
    regs[0] = issue(nr::RT_SIGPROCMASK, &args);
}

/// `rt_sigaction(sig, act, oact, sigsetsize)`: the guest's SIGSYS
/// disposition is recorded, never installed; every other handler loses
/// SIGSYS from its `sa_mask`.
fn sigaction(regs: &mut [u64; 31]) {
    let signal = regs[0] as usize;
    let act = regs[1] as *const KernelSigaction;
    let oact = regs[2] as *mut KernelSigaction;
    let mut args = args_of(regs);
    if signal == sys::SIGSYS {
        let record = GUEST_SIGSYS.get();
        // SAFETY: guest pointers as handed to the kernel; the record is a
        // plain static only this function writes.
        unsafe {
            if !oact.is_null() {
                oact.write_unaligned(core::ptr::read(record));
            }
            if !act.is_null() {
                core::ptr::write(record, act.read_unaligned());
            }
        }
        regs[0] = 0;
        return;
    }
    if act.is_null() || regs[3] != 8 {
        regs[0] = issue(nr::RT_SIGACTION, &args);
        return;
    }
    // SAFETY: a guest `struct sigaction`, as handed to the kernel.
    let mut copy = unsafe { act.read_unaligned() };
    copy.mask &= !sys::SIGSYS_BIT;
    args[1] = &copy as *const KernelSigaction as usize;
    regs[0] = issue(nr::RT_SIGACTION, &args);
}

/// `open("/proc/self/auxv")`: the kernel's copy describes the loader
/// (its `AT_PHDR`, `AT_ENTRY`, `AT_EXECFN`), so hand out a memfd holding
/// the vector the guest actually started with. rustix-based programs
/// (uutils among them) read `AT_EXECFN` this way instead of `argv[0]`.
fn serve_auxv(regs: &[u64; 31]) -> u64 {
    let config = config::get();
    if config.auxv_ptr == 0 {
        return issue(nr::OPENAT, &args_of(regs));
    }
    let flags = regs[2] as usize;
    if flags & sys::O_ACCMODE != sys::O_RDONLY {
        return sys::neg_errno(sys::EACCES);
    }
    let fd = match sys::memfd_create(c"auxv".as_ptr().cast(), sys::MFD_CLOEXEC) {
        Ok(fd) => fd,
        Err(errno) => return sys::neg_errno(errno),
    };
    // SAFETY: recorded by `config::set_auxv` from the vector the loader
    // built; it outlives the guest.
    let bytes = unsafe { core::slice::from_raw_parts(config.auxv_ptr as *const u8, config.auxv_len) };
    let mut written = 0usize;
    while written < bytes.len() {
        let count = sys::write(fd, &bytes[written..]);
        if count <= 0 {
            if let Err(_errno) = sys::close(fd) {}
            return sys::neg_errno(sys::EFAULT);
        }
        written += count as usize;
    }
    if let Err(errno) = sys::lseek(fd, 0, sys::SEEK_SET) {
        if let Err(_errno) = sys::close(fd) {}
        return sys::neg_errno(errno);
    }
    if flags & sys::O_CLOEXEC == 0
        && let Err(errno) = sys::fcntl(fd, sys::F_SETFD, 0)
    {
        if let Err(_errno) = sys::close(fd) {}
        return sys::neg_errno(errno);
    }
    fd as u64
}

/// Write `bytes` into a guest buffer with `readlink` semantics: no NUL,
/// silently truncated, returns the count.
fn copy_out(buffer: *mut u8, size: usize, bytes: &[u8]) -> u64 {
    if buffer.is_null() {
        return sys::neg_errno(sys::EFAULT);
    }
    let count = bytes.len().min(size);
    // SAFETY: a guest buffer of `size` bytes, as handed to the kernel.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, count) };
    count as u64
}

/// A link target the kernel returned is a host path when it is absolute
/// (the installer writes binding targets that way); map it back.
fn detranslate_readlink(translator: &Translator<'static>, regs: &mut [u64; 31], scratch: &mut [u8; MAX_PATH]) {
    let written = regs[0] as i64;
    if written <= 0 {
        return;
    }
    let buffer = regs[2] as *mut u8;
    let size = regs[3] as usize;
    // SAFETY: the kernel just wrote `written` bytes there.
    let target = unsafe { core::slice::from_raw_parts(buffer, written as usize) };
    if target.first() != Some(&b'/') {
        return;
    }
    let Ok(len) = translator.host_to_guest(target, scratch) else {
        return;
    };
    let count = len.min(size);
    // SAFETY: `scratch` and the guest buffer are distinct allocations.
    unsafe { core::ptr::copy_nonoverlapping(scratch.as_ptr(), buffer, count) };
    regs[0] = count as u64;
}
