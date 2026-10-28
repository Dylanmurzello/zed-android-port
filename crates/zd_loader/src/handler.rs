//! The SIGSYS trap handler and the seccomp filter that feeds it.
//!
//! The kernel delivers a tagged SIGSYS for every path syscall the guest
//! issues; the handler translates the path argument, re-issues the
//! syscall from loader text (which the filter allows by instruction
//! pointer) and writes the result into the saved `x0`. Untagged SIGSYS
//! come from Android's own per-app filter and are answered from the
//! compat table (`ENOSYS`, `EPERM`, or success under fake root).

use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use zd_engine_core::bpf::{self, ArgTrap, FilterSpec, SockFilter};
use zd_engine_core::identity::{Identity, MAX_GROUPS};
use zd_engine_core::compat::{self, Response};
use zd_engine_core::path::{MAX_PATH, Writer, is_absolute};
use zd_engine_core::stack::AT_NULL;
use zd_engine_core::sysno::ptrace as ptrace_traps;
use zd_engine_core::sysno::{
    MAX_TRAPPED, PROBE_ACK, PROBE_MAGIC, TAG_PATH, TAG_PROBE, TRAP_TABLE, TRAP_TABLE_FAKE_ROOT,
    clone as clone_traps, ioctl, is_engine_tag, name as syscall_name, nr, socket as socket_traps,
};
use zd_engine_core::translate::{ProcExe, Translated, Translator};
use zd_engine_core::{LOADER_IMAGE_BASE, LOADER_WINDOW_SIZE};

use crate::config;
use crate::exec;
use crate::link;
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
        arg_traps: &[
            ArgTrap {
                nr: nr::IOCTL,
                arg: 1,
                values: ioctl::TERMIOS2_REQUESTS,
                unless: false,
            },
            ArgTrap {
                nr: nr::SOCKET,
                arg: 0,
                values: socket_traps::TRAPPED_FAMILIES,
                unless: false,
            },
            ArgTrap {
                nr: nr::CLONE3,
                arg: 1,
                values: clone_traps::PASSED_SIZES,
                unless: true,
            },
            ArgTrap {
                nr: nr::PTRACE,
                arg: 0,
                values: ptrace_traps::TRAPPED_REQUESTS,
                unless: false,
            },
        ],
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
    let UContext {
        uc_sigmask,
        uc_mcontext,
        ..
    } = context;
    let SigContext { regs, pc, .. } = uc_mcontext;
    if tag == TAG_PROBE {
        regs[0] = PROBE_ACK;
        return;
    }
    if !is_engine_tag(tag) {
        compat_trap(number, regs);
        census(number, regs, false);
        return;
    }
    dispatch(number, regs, uc_sigmask, pc);
    census(number, regs, true);
}

/// Android's filter trapped a syscall bionic never makes.
fn compat_trap(number: u32, regs: &mut [u64; 31]) {
    COMPAT_COUNT.fetch_add(1, Ordering::Relaxed);
    if config::get().fake_root && identity_syscall(number, regs) {
        return;
    }
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

/// A refusal the guest will actually see. The engine absorbs the ones it
/// knows (a copy for a refused hard link, a classic ioctl for a termios2
/// one); whatever still reaches the guest as EACCES or EPERM is a sandbox
/// rule nothing emulates yet, and the symptom never names the syscall,
/// so this line does. ENOSYS counts only for engine traps, where it means
/// a trapped syscall the handler did not serve.
fn census(number: u32, regs: &[u64; 31], engine_trap: bool) {
    let errno = if regs[0] == sys::neg_errno(sys::EACCES) {
        sys::EACCES
    } else if regs[0] == sys::neg_errno(sys::EPERM) {
        sys::EPERM
    } else if engine_trap && regs[0] == sys::neg_errno(sys::ENOSYS) {
        sys::ENOSYS
    } else {
        return;
    };
    if config::get().log.is_empty() {
        return;
    }
    log_refusal(number, errno, regs);
}

#[inline(never)]
fn log_refusal(number: u32, errno: i32, regs: &[u64; 31]) {
    let mut line = log::Line::new();
    line.push(b"zdload: refused exe=");
    line.push(config::get().guest_exe.as_bytes());
    line.push(b" syscall=");
    match syscall_name(number) {
        Some(name) => line.push(name.as_bytes()),
        None => line.push_u64(u64::from(number)),
    }
    line.push(b" errno=");
    line.push_u64(errno as u64);
    if errno != sys::ENOSYS {
        // Only a syscall the kernel answered has had its pointers validated.
        for index in guest_path_args(number).into_iter().flatten() {
            let Some(path) = (unsafe { cstr_bounded(regs[index] as *const u8, MAX_PATH) }) else {
                continue;
            };
            line.push(b" path=");
            line.push(path);
        }
    }
    line.push(b"\n");
    log::append_to_log_file(line.as_bytes());
}

/// Registers holding the guest's path arguments.
fn guest_path_args(number: u32) -> [Option<usize>; 2] {
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
        | nr::CHDIR
        | nr::CHROOT
        | nr::EXECVE => [Some(0), None],
        nr::INOTIFY_ADD_WATCH
        | nr::MKNODAT
        | nr::MKDIRAT
        | nr::UNLINKAT
        | nr::FACCESSAT
        | nr::FCHMODAT
        | nr::FCHOWNAT
        | nr::OPENAT
        | nr::OPENAT2
        | nr::FACCESSAT2
        | nr::READLINKAT
        | nr::NEWFSTATAT
        | nr::UTIMENSAT
        | nr::NAME_TO_HANDLE_AT
        | nr::STATX
        | nr::EXECVEAT => [Some(1), None],
        nr::SYMLINKAT => [Some(2), None],
        nr::LINKAT | nr::RENAMEAT | nr::RENAMEAT2 => [Some(1), Some(3)],
        _ => [None, None],
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

/// `blocked` is the frame's `uc_sigmask`: the interrupted thread's signal
/// mask, which `rt_sigreturn` reinstalls when this handler returns. `pc`
/// is where the guest resumes: the instruction after its `svc`.
fn dispatch(number: u32, regs: &mut [u64; 31], blocked: &mut u64, pc: &mut u64) {
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
        | nr::OPENAT2
        | nr::FACCESSAT2
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
        nr::RT_SIGPROCMASK => sigprocmask(regs, blocked),
        nr::IOCTL => termios2(regs),
        nr::SOCKET => socket(regs),
        nr::CLONE3 => clone3(regs, pc),
        nr::PTRACE => traceme(regs),
        nr::KILL | nr::TKILL | nr::TGKILL | nr::RT_SIGQUEUEINFO | nr::RT_TGSIGQUEUEINFO => {
            self_signal(number, regs)
        }
        nr::FSTAT => {
            regs[0] = issue(number, &args_of(regs));
            fixup_stat(regs[0], regs[1] as *mut u8, false);
        }
        nr::FCHOWN => {
            let result = issue(number, &args_of(regs));
            regs[0] = if result == sys::neg_errno(sys::EPERM) { 0 } else { result };
        }
        nr::CAPSET => regs[0] = 0,
        _ => {
            if !identity_syscall(number, regs) {
                regs[0] = sys::neg_errno(sys::ENOSYS);
            }
        }
    }
}

fn identity() -> &'static Identity {
    &config::get().identity
}

fn identity_mut() -> &'static mut Identity {
    // SAFETY: only SIGSYS handlers of this process touch the record, and
    // glibc's setxid broadcast makes every thread write the same values.
    unsafe { &mut config::get_mut().identity }
}

fn status(result: Result<(), i32>) -> u64 {
    match result {
        Ok(()) => 0,
        Err(errno) => sys::neg_errno(errno),
    }
}

/// The uid/gid family under fake root, answered from the virtual
/// record. Returns false for any other syscall.
fn identity_syscall(number: u32, regs: &mut [u64; 31]) -> bool {
    let (a0, a1, a2) = (regs[0] as u32, regs[1] as u32, regs[2] as u32);
    regs[0] = match number {
        nr::GETUID => u64::from(identity().ruid),
        nr::GETEUID => u64::from(identity().euid),
        nr::GETGID => u64::from(identity().rgid),
        nr::GETEGID => u64::from(identity().egid),
        nr::GETRESUID => {
            let id = identity();
            write_triple(regs, [id.ruid, id.euid, id.suid])
        }
        nr::GETRESGID => {
            let id = identity();
            write_triple(regs, [id.rgid, id.egid, id.sgid])
        }
        nr::GETGROUPS => getgroups(regs),
        nr::SETUID => status(identity_mut().setuid(a0)),
        nr::SETGID => status(identity_mut().setgid(a0)),
        nr::SETREUID => status(identity_mut().setreuid(a0, a1)),
        nr::SETREGID => status(identity_mut().setregid(a0, a1)),
        nr::SETRESUID => status(identity_mut().setresuid(a0, a1, a2)),
        nr::SETRESGID => status(identity_mut().setresgid(a0, a1, a2)),
        nr::SETFSUID => u64::from(identity_mut().setfsuid(a0)),
        nr::SETFSGID => u64::from(identity_mut().setfsgid(a0)),
        nr::SETGROUPS => setgroups(regs),
        _ => return false,
    };
    true
}

/// `getresuid`/`getresgid`: three output pointers, EFAULT for a null
/// one like the kernel.
fn write_triple(regs: &[u64; 31], values: [u32; 3]) -> u64 {
    for (&pointer, value) in regs[0..3].iter().zip(values) {
        let pointer = pointer as *mut u32;
        if pointer.is_null() {
            return sys::neg_errno(sys::EFAULT);
        }
        // SAFETY: a guest output pointer, as handed to the kernel.
        unsafe { pointer.write_unaligned(value) };
    }
    0
}

/// `getgroups(size, list)`: size 0 asks for the count.
fn getgroups(regs: &[u64; 31]) -> u64 {
    let size = regs[0] as i32;
    let list = regs[1] as *mut u32;
    let groups = identity().groups();
    if size < 0 {
        return sys::neg_errno(sys::EINVAL);
    }
    if size == 0 {
        return groups.len() as u64;
    }
    if (size as usize) < groups.len() {
        return sys::neg_errno(sys::EINVAL);
    }
    if list.is_null() {
        return sys::neg_errno(sys::EFAULT);
    }
    for (index, &group) in groups.iter().enumerate() {
        // SAFETY: the guest supplied `size` slots at `list`.
        unsafe { list.add(index).write_unaligned(group) };
    }
    groups.len() as u64
}

/// `setgroups(size, list)`.
fn setgroups(regs: &[u64; 31]) -> u64 {
    let size = regs[0] as i32;
    let list = regs[1] as *const u32;
    if !identity().may_setgroups() {
        return sys::neg_errno(sys::EPERM);
    }
    if size < 0 || size as usize > MAX_GROUPS {
        return sys::neg_errno(sys::EINVAL);
    }
    let count = size as usize;
    let mut groups = [0u32; MAX_GROUPS];
    if count > 0 {
        if list.is_null() {
            return sys::neg_errno(sys::EFAULT);
        }
        for (index, slot) in groups[..count].iter_mut().enumerate() {
            // SAFETY: the guest supplied `size` ids at `list`.
            *slot = unsafe { list.add(index).read_unaligned() };
        }
    }
    status(identity_mut().setgroups(&groups[..count]))
}

/// Syscalls with one path argument at `index` whose other arguments pass
/// through unchanged, plus their result fixups.
fn path_syscall(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31], index: usize) {
    if number == nr::UTIMENSAT && regs[1] == 0 {
        // `futimens`: the fd itself, nothing to translate.
        regs[0] = issue(number, &args_of(regs));
        return;
    }
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
        Ok(Translated::ProcAuxv(who)) if number == nr::OPENAT => {
            serve_auxv(who, number, regs, regs[2] as usize)
        }
        Ok(Translated::ProcAuxv(who)) if number == nr::OPENAT2 => {
            serve_auxv(who, number, regs, open_how_flags(regs[2] as *const u64))
        }
        Ok(Translated::ProcAuxv(_)) => issue(number, &args),
        Err(_) => sys::neg_errno(sys::ENAMETOOLONG),
    };
    regs[0] = result;
    match number {
        nr::READLINKAT => detranslate_readlink(translator, regs, &mut host),
        nr::NEWFSTATAT => fixup_stat(result, regs[2] as *mut u8, false),
        nr::STATX => fixup_stat(result, regs[4] as *mut u8, true),
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

/// `clone3(args, size)` with a size other than VER0's. glibc's
/// posix_spawn passes `CLONE_CLEAR_SIGHAND`, which resets every handler in
/// the vfork child, this one included; the child's first trapped syscall
/// then kills it and, through the shared address space, the parent and
/// all its threads. The flag is cleared in the guest's struct and the
/// `svc` is re-executed with the VER0 size, which the filter lets through,
/// so the child inherits this handler the way a `vfork` child does.
/// Nothing is issued from here: a syscall that returns twice has to
/// return into the guest, not into this handler.
fn clone3(regs: &mut [u64; 31], pc: &mut u64) {
    let args = regs[0] as *mut u8;
    let size = regs[1] as usize;
    let ver0 = clone_traps::CLONE_ARGS_SIZE_VER0 as usize;
    if args.is_null() {
        regs[0] = sys::neg_errno(sys::EFAULT);
        return;
    }
    if size < ver0 {
        regs[0] = sys::neg_errno(sys::EINVAL);
        return;
    }
    if size > 4096 {
        regs[0] = sys::neg_errno(sys::E2BIG);
        return;
    }
    // The shorter form drops set_tid and cgroup; refuse rather than lose
    // them (nothing an IDE runs sets either).
    for offset in ver0..size {
        // SAFETY: a guest `struct clone_args` of `size` bytes, as handed
        // to the kernel.
        if unsafe { args.add(offset).read() } != 0 {
            regs[0] = sys::neg_errno(sys::EINVAL);
            return;
        }
    }
    let flags = args as *mut u64;
    // SAFETY: as above; `flags` is the first field.
    unsafe {
        let value = flags.read_unaligned();
        if value & clone_traps::CLONE_CLEAR_SIGHAND != 0 {
            flags.write_unaligned(value & !clone_traps::CLONE_CLEAR_SIGHAND);
        }
    }
    regs[1] = clone_traps::CLONE_ARGS_SIZE_VER0;
    *pc -= 4;
}

/// `socket(AF_NETLINK, type, protocol)`: see `sysno::socket`.
fn socket(regs: &mut [u64; 31]) {
    let result = issue(nr::SOCKET, &args_of(regs));
    let refused = result == sys::neg_errno(sys::EACCES) || result == sys::neg_errno(sys::EPERM);
    regs[0] = if refused {
        sys::neg_errno(sys::EPROTONOSUPPORT)
    } else {
        result
    };
}

/// `open_how.flags` of an `openat2` call.
fn open_how_flags(how: *const u64) -> usize {
    if how.is_null() {
        return 0;
    }
    // SAFETY: a guest `struct open_how`, as handed to the kernel.
    unsafe { how.read_unaligned() as usize }
}

/// Two corrections to a `struct stat` (uid 24, gid 28, nlink 20, ino 8)
/// or `struct statx` (uid 20, gid 24, nlink 16, ino 32) the kernel just
/// filled. Under fake root, files the app owns are presented as owned
/// by the guest's saved ids, so a process that dropped to a user for good
/// sees its files as that user's while one that only lowered its
/// effective id still sees root's (proot's fake_id0 makes the same
/// choice). Always, a file this process "hard-linked" by copying reports
/// the link count the link would have given it.
fn fixup_stat(result: u64, buffer: *mut u8, statx: bool) {
    if result != 0 || buffer.is_null() {
        return;
    }
    let config = config::get();
    let (uid_offset, gid_offset, nlink_offset, ino_offset) =
        if statx { (20usize, 24usize, 16usize, 32usize) } else { (24usize, 28usize, 20usize, 8usize) };
    // SAFETY: the kernel just filled this guest buffer.
    unsafe {
        if config.fake_root {
            let uid = buffer.add(uid_offset) as *mut u32;
            let gid = buffer.add(gid_offset) as *mut u32;
            if uid.read_unaligned() == config.real_uid {
                uid.write_unaligned(config.identity.suid);
            }
            if gid.read_unaligned() == config.real_gid {
                gid.write_unaligned(config.identity.sgid);
            }
        }
        let ino = (buffer.add(ino_offset) as *const u64).read_unaligned();
        let extra = link::extra_links(ino);
        if extra != 0 {
            let nlink = buffer.add(nlink_offset) as *mut u32;
            nlink.write_unaligned(nlink.read_unaligned().saturating_add(extra));
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
    let result = issue(number, &args);
    let refused = result == sys::neg_errno(sys::EACCES) || result == sys::neg_errno(sys::EPERM);
    regs[0] = if number == nr::LINKAT && refused {
        link::by_copy(args[0] as isize, args[1] as *const u8, args[2] as isize, args[3] as *const u8, args[4])
    } else {
        result
    };
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

/// Kernel `struct termios` (asm-generic): 36 bytes, no speed fields.
#[repr(C)]
#[derive(Clone, Copy)]
struct KernelTermios {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    line: u8,
    cc: [u8; 19],
}

/// Kernel `struct termios2`: the same plus explicit input and output
/// speeds, 44 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct KernelTermios2 {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    line: u8,
    cc: [u8; 19],
    ispeed: u32,
    ospeed: u32,
}

const _: () = {
    assert!(size_of::<KernelTermios>() == 36);
    assert!(size_of::<KernelTermios2>() == 44);
};

const CBAUD: u32 = 0x100f;
const BOTHER: u32 = 0x1000;
const BAUD_LOW: [u32; 16] = [
    0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400,
];
const BAUD_HIGH: [u32; 15] = [
    57600, 115200, 230400, 460800, 500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000,
    2500000, 3000000, 3500000, 4000000,
];

fn speed_of(cflag: u32) -> u32 {
    let code = cflag & CBAUD;
    if code < 16 {
        BAUD_LOW[code as usize]
    } else if (0x1001..=0x100f).contains(&code) {
        BAUD_HIGH[(code - 0x1001) as usize]
    } else {
        0
    }
}

/// Closest classic `Bxxx` code for a speed, for a `BOTHER` request that
/// a pty would ignore anyway.
fn code_of(speed: u32) -> u32 {
    if let Some(index) = BAUD_LOW.iter().position(|&candidate| candidate == speed) {
        return index as u32;
    }
    if let Some(index) = BAUD_HIGH.iter().position(|&candidate| candidate == speed) {
        return 0x1001 + index as u32;
    }
    0xf
}

/// `ioctl(fd, TCGETS2 | TCSETS2 | TCSETSW2 | TCSETSF2, termios2*)`, served
/// through the classic requests the app domain is allowed. glibc 2.41+
/// uses only the termios2 forms, so without this no guest can tell a
/// terminal from a pipe.
fn termios2(regs: &mut [u64; 31]) {
    let fd = regs[0] as usize;
    let request = regs[1];
    let argument = regs[2] as *mut u8;
    if argument.is_null() {
        regs[0] = sys::neg_errno(sys::EFAULT);
        return;
    }
    if request == ioctl::TCGETS2 {
        let mut classic = KernelTermios {
            iflag: 0,
            oflag: 0,
            cflag: 0,
            lflag: 0,
            line: 0,
            cc: [0; 19],
        };
        let result = unsafe {
            sys::syscall6(
                nr::IOCTL,
                fd,
                ioctl::TCGETS as usize,
                &mut classic as *mut KernelTermios as usize,
                0,
                0,
                0,
            )
        };
        if result != 0 {
            regs[0] = not_a_tty(result as u64);
            return;
        }
        let speed = speed_of(classic.cflag);
        let extended = KernelTermios2 {
            iflag: classic.iflag,
            oflag: classic.oflag,
            cflag: classic.cflag,
            lflag: classic.lflag,
            line: classic.line,
            cc: classic.cc,
            ispeed: speed,
            ospeed: speed,
        };
        // SAFETY: a guest `struct termios2`, as handed to the kernel.
        unsafe { (argument as *mut KernelTermios2).write_unaligned(extended) };
        regs[0] = 0;
        return;
    }
    let classic_request = match request {
        ioctl::TCSETS2 => ioctl::TCSETS,
        ioctl::TCSETSW2 => ioctl::TCSETSW,
        ioctl::TCSETSF2 => ioctl::TCSETSF,
        _ => {
            regs[0] = issue(nr::IOCTL, &args_of(regs));
            return;
        }
    };
    // SAFETY: a guest `struct termios2`, as handed to the kernel.
    let extended = unsafe { (argument as *const KernelTermios2).read_unaligned() };
    let mut cflag = extended.cflag;
    if cflag & CBAUD == BOTHER {
        cflag = (cflag & !CBAUD) | code_of(extended.ospeed);
    }
    let classic = KernelTermios {
        iflag: extended.iflag,
        oflag: extended.oflag,
        cflag,
        lflag: extended.lflag,
        line: extended.line,
        cc: extended.cc,
    };
    regs[0] = not_a_tty(unsafe {
        sys::syscall6(
            nr::IOCTL,
            fd,
            classic_request as usize,
            &classic as *const KernelTermios as usize,
            0,
            0,
            0,
        ) as u64
    });
}

/// A terminal request the sandbox refuses on a socket (its ioctl
/// allowlist answers EACCES) is, to the guest, a request on something
/// that is not a terminal.
fn not_a_tty(result: u64) -> u64 {
    if result == sys::neg_errno(sys::EACCES) {
        sys::neg_errno(sys::ENOTTY)
    } else {
        result
    }
}

/// `rt_sigprocmask(how, set, oldset, sigsetsize)`, applied to the signal
/// frame rather than re-issued. The kernel reinstalls `uc_sigmask` at
/// `rt_sigreturn`, so a mask changed by a syscall from inside this
/// handler would be undone the moment the guest resumes (bash blocks
/// SIGTTOU around `tcsetpgrp` this way; with the change undone every
/// foreground job stopped in the child's `tcsetpgrp`). SIGSYS never
/// enters the blocked set, whatever the guest asks: a trap while it is
/// blocked kills the process.
fn sigprocmask(regs: &mut [u64; 31], blocked: &mut u64) {
    let how = regs[0] as usize;
    let set = regs[1] as *const u64;
    let oldset = regs[2] as *mut u64;
    if regs[3] != 8 {
        regs[0] = sys::neg_errno(sys::EINVAL);
        return;
    }
    let old = *blocked;
    if !set.is_null() {
        // SAFETY: a guest sigset of 8 bytes, as handed to the kernel.
        let requested = unsafe { set.read_unaligned() };
        let new = match how {
            sys::SIG_BLOCK => old | requested,
            sys::SIG_UNBLOCK => old & !requested,
            sys::SIG_SETMASK => requested,
            _ => {
                regs[0] = sys::neg_errno(sys::EINVAL);
                return;
            }
        };
        *blocked = new & !sys::SIGSYS_BIT;
    }
    if !oldset.is_null() {
        // SAFETY: a guest sigset of 8 bytes, as handed to the kernel.
        unsafe { oldset.write_unaligned(old) };
    }
    regs[0] = 0;
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

/// Largest `/proc/<pid>/auxv` served for another process; the kernel
/// itself caps a vector at `AT_VECTOR_SIZE`, under 32 entries on arm64.
const MAX_AUXV_BYTES: usize = 64 * 16;

/// `open("/proc/<who>/auxv")`: the kernel's copy describes the loader
/// (its `AT_PHDR`, `AT_ENTRY`, `AT_EXECFN`), so hand out a memfd holding
/// the vector the guest actually started with. rustix-based programs
/// (uutils among them) read `AT_EXECFN` this way instead of `argv[0]`,
/// and a debugger reads its tracee's vector to place the executable
/// (lldb-server takes the load bias from `AT_ENTRY` and finds the
/// dynamic linker rendezvous through `AT_PHDR`).
fn serve_auxv(who: ProcExe, number: u32, regs: &[u64; 31], flags: usize) -> u64 {
    let mut remote = [0u8; MAX_AUXV_BYTES];
    let bytes: &[u8] = if is_self(who) {
        let config = config::get();
        if config.auxv_ptr == 0 {
            return issue(number, &args_of(regs));
        }
        // SAFETY: recorded by `config::set_auxv` from the vector the loader
        // built; it outlives the guest.
        unsafe { core::slice::from_raw_parts(config.auxv_ptr as *const u8, config.auxv_len) }
    } else {
        let ProcExe::Pid(pid) = who else {
            return issue(number, &args_of(regs));
        };
        match remote_auxv(pid, &mut remote) {
            Some(length) => &remote[..length],
            None => return issue(number, &args_of(regs)),
        }
    };
    if flags & sys::O_ACCMODE != sys::O_RDONLY {
        return sys::neg_errno(sys::EACCES);
    }
    let fd = match sys::memfd_create(c"auxv".as_ptr().cast(), sys::MFD_CLOEXEC) {
        Ok(fd) => fd,
        Err(errno) => return sys::neg_errno(errno),
    };
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

/// The vector another engine process started with, read out of that
/// process: `Config` is a static of this non-PIE image, so its fields
/// sit at the same addresses in every process running this build. A
/// process of another build, or one not running the loader, yields
/// bytes that fail the shape checks, and the caller serves the kernel's
/// file as before.
fn remote_auxv(pid: u32, buffer: &mut [u8; MAX_AUXV_BYTES]) -> Option<usize> {
    let config = config::get();
    let mut pointer = 0usize;
    let mut length = 0usize;
    let local = [
        sys::IoVec { base: &mut pointer as *mut usize as usize, len: size_of::<usize>() },
        sys::IoVec { base: &mut length as *mut usize as usize, len: size_of::<usize>() },
    ];
    let remote = [
        sys::IoVec { base: &config.auxv_ptr as *const usize as usize, len: size_of::<usize>() },
        sys::IoVec { base: &config.auxv_len as *const usize as usize, len: size_of::<usize>() },
    ];
    if sys::process_vm_readv(pid, &local, &remote) != Ok(2 * size_of::<usize>()) {
        return None;
    }
    if pointer == 0
        || !pointer.is_multiple_of(8)
        || length == 0
        || !length.is_multiple_of(16)
        || length > buffer.len()
    {
        return None;
    }
    let local = [sys::IoVec { base: buffer.as_mut_ptr() as usize, len: length }];
    let remote = [sys::IoVec { base: pointer, len: length }];
    if sys::process_vm_readv(pid, &local, &remote) != Ok(length) {
        return None;
    }
    let entries = length / 16;
    for index in 0..entries {
        let offset = index * 16;
        let key = u64::from_ne_bytes(buffer[offset..offset + 8].try_into().unwrap_or([0; 8]));
        if (key == AT_NULL) != (index + 1 == entries) {
            return None;
        }
    }
    Some(length)
}

/// Set by `ptrace(PTRACE_TRACEME)` and consumed by the next `execve`,
/// which carries it to the loader as `env::TRACEME`.
pub static TRACEME_REQUESTED: AtomicBool = AtomicBool::new(false);

/// `ptrace(PTRACE_TRACEME)`: answered here and performed by the loader
/// after the exec that follows. Attaching now would turn every SIGSYS
/// between this call and the exec into a ptrace stop that the parent,
/// blocked on its exec pipe, never resumes (lldb-server and gdb both
/// launch that way), and a tracer looking at the process at its first stop
/// would read the loader's auxiliary vector instead of the guest's.
fn traceme(regs: &mut [u64; 31]) {
    if regs[0] == ptrace_traps::TRACEME {
        TRACEME_REQUESTED.store(true, Ordering::Relaxed);
        regs[0] = 0;
    } else {
        regs[0] = issue(nr::PTRACE, &args_of(regs));
    }
}

/// A signal the guest sends while a `PTRACE_TRACEME` is pending. The
/// tracer is waiting for the ptrace stop this signal causes, so the attach
/// cannot wait for an exec that may never come: lldb-server's single-step
/// check forks a child that calls TRACEME and raises SIGSTOP in a loop,
/// while gdb-style launchers only exec. Attached first, the signal lands
/// as a ptrace stop; a signal to another process changes nothing.
fn self_signal(number: u32, regs: &mut [u64; 31]) {
    if TRACEME_REQUESTED.load(Ordering::Relaxed) && may_reach_self(number, regs) {
        TRACEME_REQUESTED.store(false, Ordering::Relaxed);
        if let Err(errno) = sys::ptrace(sys::PTRACE_TRACEME, 0, 0, 0) {
            let mut line = log::Line::new();
            line.push(b"zdload: ptrace(PTRACE_TRACEME) before a signal to self failed: errno ");
            line.push_u64(errno as u64);
            line.emit();
        }
    }
    regs[0] = issue(number, &args_of(regs));
}

/// Whether the signal's target set includes the caller: `kill` and
/// `rt_sigqueueinfo` take a pid, where 0 is the caller's group, -1 is
/// everyone but the caller and other negatives name a group; `tkill` a
/// thread id; `tgkill` and `rt_tgsigqueueinfo` a thread-group id.
fn may_reach_self(number: u32, regs: &[u64; 31]) -> bool {
    let first = regs[0] as u32 as i32;
    match number {
        nr::TKILL => first as u32 == sys::gettid(),
        nr::TGKILL | nr::RT_TGSIGQUEUEINFO => first as u32 == sys::getpid(),
        _ => match first {
            0 => true,
            -1 => false,
            pid if pid > 0 => pid as u32 == sys::getpid(),
            group => (-group) as u32 == sys::getpgid(0),
        },
    }
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
