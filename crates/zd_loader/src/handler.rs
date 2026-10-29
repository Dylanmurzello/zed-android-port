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
use zd_engine_core::path::{MAX_PATH, Writer, is_absolute, strip_prefix};
use zd_engine_core::procfs;
use zd_engine_core::stack::AT_NULL;
use zd_engine_core::sysno::address_arg;
use zd_engine_core::sysno::prctl as prctl_traps;
use zd_engine_core::sysno::ptrace as ptrace_traps;
use zd_engine_core::sysno::sockopt as sockopt_traps;
use zd_engine_core::sysno::{
    MAX_TRAPPED, PROBE_ACK, PROBE_MAGIC, TAG_PATH, TAG_PROBE, TRAP_TABLE, TRAP_TABLE_FAKE_ROOT, TRAP_TABLE_UNAME,
    clone as clone_traps, ioctl, is_engine_tag, name as syscall_name, nr, socket as socket_traps,
};
use zd_engine_core::translate::{ProcExe, Translated, Translator};
use zd_engine_core::{LOADER_IMAGE_BASE, LOADER_WINDOW_SIZE};

use crate::config;
use crate::exec;
use crate::link;
use crate::scratch::{self, Scratch, Working};
use crate::socket;
use crate::log;
use crate::procfs as served_proc;
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
        // SA_ONSTACK: a Go program runs its goroutines on stacks of a
        // few KiB that cannot hold the kernel's signal frame, and its
        // runtime registers an alternate stack on every thread for that
        // reason; a thread without one is served on its own stack as
        // before.
        flags: sys::SA_SIGINFO | sys::SA_NODEFER | sys::SA_RESTART | sys::SA_ONSTACK,
        restorer: 0,
        // Every other signal waits until the handler returns. A guest
        // handler that ran in between would see this code's registers:
        // Go keeps the current goroutine in x28 and its handler reads
        // the register as it finds it, so a preemption signal landing
        // inside a trap crashed every multi-threaded Go program.
        mask: !sys::SIGSYS_BIT,
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
    if !config::get().hostname.is_empty() || !config::get().uname_release.is_empty() {
        for &number in TRAP_TABLE_UNAME {
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
            ArgTrap {
                nr: nr::GETSOCKOPT,
                arg: 2,
                values: sockopt_traps::TRAPPED_NAMES,
                unless: false,
            },
            ArgTrap {
                nr: nr::RECVFROM,
                arg: 4,
                values: address_arg::NULL,
                unless: true,
            },
            ArgTrap {
                nr: nr::PRCTL,
                arg: 0,
                values: prctl_traps::TRAPPED_OPTIONS,
                unless: false,
            },
            ArgTrap {
                nr: nr::ACCEPT4,
                arg: 1,
                values: address_arg::NULL,
                unless: true,
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
    let (si_code, tag, number) =
        unsafe { ((*info).si_code, (*info).si_errno as u16, (*info).si_syscall as u32) };
    if si_code != SYS_SECCOMP {
        return;
    }
    TRAP_COUNT.fetch_add(1, Ordering::Relaxed);
    if tag != TAG_PROBE && !is_engine_tag(tag) && forward_to_guest(number, info, context) {
        return;
    }
    // SAFETY: as above; a guest handler, if one ran, has returned.
    let context = unsafe { &mut *context };
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
    // The call's own x0: the result replaces it, and the refusal log
    // reads a path from it.
    let first_argument = regs[0];
    if !is_engine_tag(tag) {
        compat_trap(number, regs);
        census(number, regs, first_argument, false);
        return;
    }
    dispatch(number, regs, uc_sigmask, pc);
    census(number, regs, first_argument, true);
}

/// A SIGSYS that is not the engine's comes from Android's filter or from
/// a filter the guest installed itself, and the two carry no mark that
/// tells them apart. A syscall the compat table knows is Android's and
/// is answered there; any other goes to the handler the guest recorded
/// through the virtualized `rt_sigaction`, so a program that sandboxes
/// itself with `SECCOMP_RET_TRAP` sees its own traps. The guest handler
/// runs inside this one with the mask this one has, SIGSYS unblocked.
fn forward_to_guest(number: u32, info: *mut SigInfo, context: *mut UContext) -> bool {
    if !matches!(compat::response(number, config::get().fake_root), Response::Unknown) {
        return false;
    }
    // SAFETY: a plain static written only by the virtualized rt_sigaction.
    let record = unsafe { core::ptr::read(GUEST_SIGSYS.get()) };
    match record.handler {
        sys::SIG_DFL => false,
        sys::SIG_IGN => true,
        handler => {
            if record.flags & sys::SA_SIGINFO != 0 {
                // SAFETY: the guest installed this address as its SA_SIGINFO
                // handler for SIGSYS; it receives the kernel's own frame.
                let handler: extern "C" fn(i32, *mut SigInfo, *mut UContext) =
                    unsafe { core::mem::transmute(handler as usize) };
                handler(sys::SIGSYS as i32, info, context);
            } else {
                // SAFETY: as above, for a handler taking the signal only.
                let handler: extern "C" fn(i32) = unsafe { core::mem::transmute(handler as usize) };
                handler(sys::SIGSYS as i32);
            }
            if record.flags & sys::SA_RESETHAND != 0 {
                // SAFETY: the record is written only from SIGSYS handlers of
                // this process; the kernel would have reset it the same way.
                unsafe { (*GUEST_SIGSYS.get()).handler = sys::SIG_DFL };
            }
            true
        }
    }
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
        Response::Accept4 => accept_compat(regs),
        Response::Unknown => {
            log_unknown(number);
            sys::neg_errno(sys::ENOSYS)
        }
    };
}

/// `uname(buf)`: the kernel's answer with the configured node name and
/// release written over its fields (`struct new_utsname`, six fields of
/// 65 bytes).
fn uname(regs: &mut [u64; 31]) {
    let buffer = regs[0] as *mut u8;
    regs[0] = issue(nr::UNAME, &args_of(regs));
    if regs[0] != 0 {
        return;
    }
    let config = config::get();
    for (field, value) in [(1usize, &config.hostname), (2usize, &config.uname_release)] {
        if value.is_empty() {
            continue;
        }
        let bytes = value.as_bytes();
        let len = bytes.len().min(64);
        // SAFETY: the guest's utsname buffer, as handed to the kernel,
        // which just filled it.
        unsafe {
            let field = buffer.add(field * 65);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), field, len);
            core::ptr::write_bytes(field.add(len), 0, 65 - len);
        }
    }
}

/// `prctl(PR_GET_AUXV, buffer, size)`: the vector the guest started with,
/// in place of the kernel's copy that describes the loader. As the
/// kernel does, the buffer gets what fits and the full length comes back.
fn get_auxv(regs: &mut [u64; 31]) {
    let config = config::get();
    if config.auxv_ptr == 0 {
        regs[0] = issue(nr::PRCTL, &args_of(regs));
        return;
    }
    let buffer = regs[1] as *mut u8;
    if buffer.is_null() {
        regs[0] = sys::neg_errno(sys::EFAULT);
        return;
    }
    let copy = config.auxv_len.min(regs[2] as usize);
    // SAFETY: the vector recorded by `config::set_auxv` outlives the guest;
    // the guest's buffer holds `size` bytes, as handed to the kernel.
    unsafe { core::ptr::copy_nonoverlapping(config.auxv_ptr as *const u8, buffer, copy) };
    regs[0] = config.auxv_len as u64;
}

/// The legacy `accept` answered as `accept4` with no flags, the peer
/// address mapped back like any other.
#[inline(never)]
fn accept_compat(regs: &[u64; 31]) -> u64 {
    let mut as_accept4 = *regs;
    as_accept4[3] = 0;
    let Some(translator) = config::translator() else {
        return issue(nr::ACCEPT4, &args_of(&as_accept4));
    };
    let Some(mut claim) = scratch::claim() else {
        return issue(nr::ACCEPT4, &args_of(&as_accept4));
    };
    socket::address_out(translator, nr::ACCEPT4, &mut as_accept4, 1, 2, claim.get());
    as_accept4[0]
}

#[inline(never)]
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
fn census(number: u32, regs: &[u64; 31], first_argument: u64, engine_trap: bool) {
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
    log_refusal(number, errno, regs, first_argument);
}

#[inline(never)]
fn log_refusal(number: u32, errno: i32, regs: &[u64; 31], first_argument: u64) {
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
            let argument = if index == 0 { first_argument } else { regs[index] };
            let Some(path) = (unsafe { cstr_bounded(argument as *const u8, MAX_PATH) }) else {
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

/// `chroot(dir)` under fake root: the process, and every child it execs,
/// translates against `dir` from now on; the identity bindings (`/dev`,
/// `/proc`, `/sys`) stay, the way they would after the mounts a real
/// chroot is usually given. The cwd is left where it is, as the kernel
/// leaves it. A relative target (the chroot utility chdirs first and
/// passes `.`) is anchored on the cwd.
#[inline(never)]
fn chroot(regs: &[u64; 31]) -> u64 {
    let Some(translator) = config::translator() else {
        return sys::neg_errno(sys::EPERM);
    };
    if !config::get().fake_root {
        return sys::neg_errno(sys::EPERM);
    }
    let Some(guest) = (unsafe { cstr_bounded(regs[0] as *const u8, MAX_PATH) }) else {
        return sys::neg_errno(if regs[0] == 0 { sys::EFAULT } else { sys::ENAMETOOLONG });
    };
    let Some(mut claim) = scratch::claim() else {
        return sys::neg_errno(sys::EPERM);
    };
    let scratch = claim.get();
    let [host, link, target, absolute, ..] = &mut scratch.paths;
    let path: &[u8] = if is_absolute(guest) {
        guest
    } else {
        let cwd_len = match sys::getcwd(link) {
            Ok(len) => len,
            Err(errno) => return sys::neg_errno(errno),
        };
        let Ok(guest_cwd_len) = translator.host_to_guest(&link[..cwd_len], target) else {
            return sys::neg_errno(sys::ENAMETOOLONG);
        };
        let mut writer = Writer::new(&mut absolute[..]);
        if writer.push(&target[..guest_cwd_len]).is_err() || writer.push_component(guest).is_err() {
            return sys::neg_errno(sys::ENAMETOOLONG);
        }
        let len = writer.finish();
        &absolute[..len]
    };
    let host_len = match translate(translator, path, host, link, target) {
        Ok(Translated::Host(len)) => len,
        Ok(_) => return sys::neg_errno(sys::EPERM),
        Err(errno) => return sys::neg_errno(errno),
    };
    match sys::stat_mode(sys::AT_FDCWD, host.as_ptr(), 0) {
        Ok(mode) if mode & sys::S_IFMT == sys::S_IFDIR => {}
        Ok(_) => return sys::neg_errno(sys::ENOTDIR),
        Err(errno) => return sys::neg_errno(errno),
    }
    if host[..host_len] == *translator.rootfs {
        return 0;
    }
    match config::rechroot(&host[..host_len]) {
        Ok(()) => 0,
        Err(errno) => sys::neg_errno(errno),
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

pub(crate) fn args_of(regs: &[u64; 31]) -> [usize; 6] {
    [
        regs[0] as usize,
        regs[1] as usize,
        regs[2] as usize,
        regs[3] as usize,
        regs[4] as usize,
        regs[5] as usize,
    ]
}

pub(crate) fn issue(number: u32, args: &[usize; 6]) -> u64 {
    unsafe { sys::syscall6(number, args[0], args[1], args[2], args[3], args[4], args[5]) as u64 }
}

/// `guest_to_host` for a syscall argument, following a `/proc` directory
/// link when the path climbs through one. The kernel resolves such a
/// link to the host directory and takes `..` from there, which can leave
/// the guest's view of the tree; so the link is read, its target mapped
/// back to the guest, the rest re-anchored and clamped there, and the
/// whole translated again. `link` and `target` are scratch.
pub(crate) fn translate(
    translator: &Translator<'static>,
    guest: &[u8],
    out: &mut [u8; MAX_PATH],
    link: &mut [u8; MAX_PATH],
    target: &mut [u8; MAX_PATH],
) -> Result<Translated, i32> {
    let link_len = match translator.guest_to_host(guest, out) {
        Ok(Translated::ProcLink { link_len }) => link_len,
        Ok(translated) => return Ok(translated),
        Err(_) => return Err(sys::ENAMETOOLONG),
    };
    if link_len >= MAX_PATH {
        return Err(sys::ENAMETOOLONG);
    }
    link[..link_len].copy_from_slice(&guest[..link_len]);
    link[link_len] = 0;
    let target_len = sys::readlinkat(sys::AT_FDCWD, link.as_ptr(), &mut target[..MAX_PATH - 1])?;
    if !is_absolute(&target[..target_len]) {
        // A socket or pipe behind the descriptor: nothing to walk.
        return Err(sys::ENOTDIR);
    }
    let dir_len = translator
        .host_to_guest(&target[..target_len], link)
        .map_err(|_| sys::ENAMETOOLONG)?;
    let mut writer = Writer::resume(link, dir_len);
    writer.push(&guest[link_len..]).map_err(|_| sys::ENAMETOOLONG)?;
    let joined_len = writer.finish();
    match translator.guest_to_host(&link[..joined_len], out) {
        Ok(Translated::ProcLink { .. }) => Err(sys::EACCES),
        Ok(translated) => Ok(translated),
        Err(_) => Err(sys::ENAMETOOLONG),
    }
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
    match scratch::claim() {
        Some(claim) => dispatch_with(translator, number, regs, blocked, pc, Working::Pooled(claim)),
        None => dispatch_on_stack(translator, number, regs, blocked, pc),
    }
}

/// The pool is exhausted (sixty-four invocations at once, or a fork
/// child that inherited the parent's busy bits): a stack copy, at the
/// frame size the pool exists to avoid.
#[inline(never)]
fn dispatch_on_stack(
    translator: &Translator<'static>,
    number: u32,
    regs: &mut [u64; 31],
    blocked: &mut u64,
    pc: &mut u64,
) {
    let mut scratch = Scratch::new();
    dispatch_with(translator, number, regs, blocked, pc, Working::Stack(&mut scratch));
}

fn dispatch_with(
    translator: &Translator<'static>,
    number: u32,
    regs: &mut [u64; 31],
    blocked: &mut u64,
    pc: &mut u64,
    mut working: Working,
) {
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
        | nr::CHDIR => path_syscall(translator, number, regs, 0, working.get()),
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
        | nr::STATX => path_syscall(translator, number, regs, 1, working.get()),
        nr::EXECVE | nr::EXECVEAT => exec::handle(number, regs, *blocked, working),
        nr::SYMLINKAT => symlinkat(translator, regs, working.get()),
        nr::LINKAT | nr::RENAMEAT | nr::RENAMEAT2 => two_paths(translator, number, regs, working.get()),
        nr::GETCWD => getcwd(translator, regs, working.get()),
        nr::BIND | nr::CONNECT => socket::address_in(translator, number, regs, 1, 2, working.get()),
        nr::SENDTO => socket::address_in(translator, number, regs, 4, 5, working.get()),
        nr::GETSOCKNAME | nr::GETPEERNAME | nr::ACCEPT4 => {
            socket::address_out(translator, number, regs, 1, 2, working.get())
        }
        nr::RECVFROM => socket::address_out(translator, number, regs, 4, 5, working.get()),
        nr::SENDMSG | nr::RECVMSG => socket::message(translator, number, regs, working.get()),
        nr::SENDMMSG | nr::RECVMMSG => socket::multi_message(translator, number, regs, working.get()),
        nr::RT_SIGACTION => sigaction(regs),
        nr::RT_SIGPROCMASK => sigprocmask(regs, blocked),
        nr::IOCTL => termios2(regs),
        nr::SOCKET => socket(regs),
        nr::CLONE3 => clone3(regs, pc),
        nr::PTRACE => traceme(regs),
        nr::GETSOCKOPT => peer_credentials(regs),
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
        nr::UNAME => uname(regs),
        nr::CHROOT => regs[0] = chroot(regs),
        nr::PRCTL => get_auxv(regs),
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
fn path_syscall(
    translator: &Translator<'static>,
    number: u32,
    regs: &mut [u64; 31],
    index: usize,
    scratch: &mut Scratch,
) {
    if number == nr::UTIMENSAT && regs[1] == 0 {
        // `futimens`: the fd itself, nothing to translate.
        regs[0] = issue(number, &args_of(regs));
        return;
    }
    let Some(guest) = (unsafe { cstr_bounded(regs[index] as *const u8, MAX_PATH) }) else {
        regs[0] = sys::neg_errno(if regs[index] == 0 { sys::EFAULT } else { sys::ENAMETOOLONG });
        return;
    };
    let [host, link, target, _, render_buffer] = &mut scratch.paths;
    let mut args = args_of(regs);
    let mut translated = translate(translator, guest, host, link, target);
    if matches!(translated, Ok(Translated::Relative)) && index == 1 {
        if let Some(special) = proc_relative(translator, regs[0] as isize, guest, host, link, target) {
            translated = Ok(special);
        }
    }
    let result = match translated {
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
            args[index] = match translator.guest_to_host(guest_exe, host) {
                Ok(Translated::Host(_)) => host.as_ptr() as usize,
                _ => config::get().guest_exe.as_ptr() as usize,
            };
            issue(number, &args)
        }
        Ok(Translated::ProcExe(_)) => issue(number, &args),
        Ok(Translated::ProcAuxv(who)) if number == nr::OPENAT => {
            serve_auxv(who, number, regs, regs[2] as usize, &mut scratch.bytes)
        }
        Ok(Translated::ProcAuxv(who)) if number == nr::OPENAT2 => {
            serve_auxv(who, number, regs, open_how_flags(regs[2] as *const u64), &mut scratch.bytes)
        }
        Ok(Translated::ProcAuxv(_)) => issue(number, &args),
        Ok(Translated::ProcSynth(file)) => {
            // The kernel's own file first: a device that allows it keeps it.
            let result = issue(number, &args);
            served_proc::serve(file, number, regs, result, &mut render_buffer[..])
        }
        Ok(Translated::ProcLink { .. }) => sys::neg_errno(sys::EACCES),
        Err(errno) => sys::neg_errno(errno),
    };
    regs[0] = result;
    match number {
        nr::READLINKAT => detranslate_readlink(translator, regs, host),
        nr::NEWFSTATAT => fixup_stat(result, regs[2] as *mut u8, false),
        nr::STATX => fixup_stat(result, regs[4] as *mut u8, true),
        nr::FCHOWNAT if result == sys::neg_errno(sys::EPERM) => regs[0] = 0,
        nr::MKNODAT if result == sys::neg_errno(sys::EPERM) => {
            regs[0] = device_placeholder(args[0] as isize, args[1] as *const u8, regs[2] as u32);
        }
        _ => {}
    }
}

/// `auxv` or `exe` named relative to a descriptor that sits on
/// `/proc/<pid>` (rustix opens the auxiliary vector that way): the same
/// special file as the absolute spelling, which a relative path would
/// otherwise reach through the kernel's own view of it.
fn proc_relative(
    translator: &Translator<'static>,
    dirfd: isize,
    name: &[u8],
    host: &mut [u8; MAX_PATH],
    link: &mut [u8; MAX_PATH],
    target: &mut [u8; MAX_PATH],
) -> Option<Translated> {
    if dirfd == sys::AT_FDCWD || (name != b"auxv" && name != b"exe") {
        return None;
    }
    let mut digits = [0u8; 20];
    let mut writer = Writer::new(&mut link[..]);
    writer.push(b"/proc/self/fd/").ok()?;
    writer.push(log::itoa(&mut digits, dirfd as u64)).ok()?;
    writer.finish();
    let dir_len = sys::readlinkat(sys::AT_FDCWD, link.as_ptr(), &mut target[..MAX_PATH - 1]).ok()?;
    let guest_dir_len = translator.host_to_guest(&target[..dir_len], link).ok()?;
    let mut writer = Writer::resume(&mut link[..], guest_dir_len);
    writer.push_component(name).ok()?;
    let len = writer.finish();
    match translator.guest_to_host(&link[..len], host) {
        Ok(special @ (Translated::ProcExe(_) | Translated::ProcAuxv(_))) => Some(special),
        _ => None,
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
pub(crate) fn open_how_flags(how: *const u64) -> usize {
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

/// `getsockopt(SOL_SOCKET, SO_PEERCRED)`: the kernel reports the peer's real
/// ids, which under fake root must read as the virtual identity the way
/// stat owners do. OpenSSH's multiplexing master compares the peer's uid
/// with its own `getuid()` and drops the client on a mismatch.
fn peer_credentials(regs: &mut [u64; 31]) {
    regs[0] = issue(nr::GETSOCKOPT, &args_of(regs));
    let config = config::get();
    if regs[0] != 0 || !config.fake_root || regs[1] != sockopt_traps::SOL_SOCKET {
        return;
    }
    let buffer = regs[3] as *mut u8;
    let length = regs[4] as *const u32;
    if buffer.is_null() || length.is_null() {
        return;
    }
    // SAFETY: the kernel just filled these guest buffers; `struct ucred` is
    // pid, uid, gid, each 32 bits.
    unsafe {
        if length.read_unaligned() < 12 {
            return;
        }
        let uid = buffer.add(4) as *mut u32;
        let gid = buffer.add(8) as *mut u32;
        if uid.read_unaligned() == config.real_uid {
            uid.write_unaligned(config.identity.suid);
        }
        if gid.read_unaligned() == config.real_gid {
            gid.write_unaligned(config.identity.sgid);
        }
    }
}

/// `linkat`, `renameat`, `renameat2`: paths at x1 and x3.
#[inline(never)]
fn two_paths(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31], scratch: &mut Scratch) {
    let mut args = args_of(regs);
    let [first, second, link, target, _] = &mut scratch.paths;
    for (index, buffer) in [(1usize, first), (3usize, second)] {
        let Some(guest) = (unsafe { cstr_bounded(regs[index] as *const u8, MAX_PATH) }) else {
            regs[0] = sys::neg_errno(if regs[index] == 0 { sys::EFAULT } else { sys::ENAMETOOLONG });
            return;
        };
        match translate(translator, guest, buffer, link, target) {
            Ok(Translated::Host(_)) => args[index] = buffer.as_ptr() as usize,
            Ok(_) => {}
            Err(errno) => {
                regs[0] = sys::neg_errno(errno);
                return;
            }
        }
    }
    let result = issue(number, &args);
    let refused = result == sys::neg_errno(sys::EACCES) || result == sys::neg_errno(sys::EPERM);
    regs[0] = if number == nr::LINKAT && refused {
        link::by_copy(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4],
            scratch,
        )
    } else {
        result
    };
}

/// Guest absolute path of the directory `dirfd` refers to, via the
/// kernel's own view of the fd.
fn guest_dir_of_fd(
    translator: &Translator<'static>,
    dirfd: isize,
    out: &mut [u8; MAX_PATH],
    host: &mut [u8; MAX_PATH],
) -> Option<usize> {
    let len = if dirfd == sys::AT_FDCWD {
        sys::getcwd(host).ok()?
    } else {
        let mut link = [0u8; 32];
        let mut writer = Writer::new(&mut link);
        writer.push(b"/proc/self/fd/").ok()?;
        let mut digits = [0u8; 20];
        writer.push(log::itoa(&mut digits, dirfd as u64)).ok()?;
        writer.finish();
        sys::readlinkat(sys::AT_FDCWD, link.as_ptr(), host).ok()?
    };
    translator.host_to_guest(&host[..len], out).ok()
}

/// `symlinkat(target, dirfd, linkpath)`: the stored target is rewritten
/// so the kernel can resolve the link from the host side.
#[inline(never)]
fn symlinkat(translator: &Translator<'static>, regs: &mut [u64; 31], scratch: &mut Scratch) {
    let (Some(target), Some(link)) = (
        unsafe { cstr_bounded(regs[0] as *const u8, MAX_PATH) },
        unsafe { cstr_bounded(regs[2] as *const u8, MAX_PATH) },
    ) else {
        regs[0] = sys::neg_errno(sys::EFAULT);
        return;
    };
    let dirfd = regs[1] as isize;
    let [link_guest, new_target, host_link, spare @ ..] = &mut scratch.paths;
    let link_guest_len = if is_absolute(link) {
        let len = link.len().min(MAX_PATH - 1);
        link_guest[..len].copy_from_slice(&link[..len]);
        len
    } else {
        let [_, dir_host] = &mut *spare;
        let Some(dir_len) = guest_dir_of_fd(translator, dirfd, link_guest, dir_host) else {
            regs[0] = sys::neg_errno(sys::EACCES);
            return;
        };
        let mut writer = Writer::resume(link_guest, dir_len);
        if writer.push_component(link).is_err() {
            regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
            return;
        }
        writer.finish()
    };
    let Ok(_) = translator.rewrite_symlink_target(&link_guest[..link_guest_len], target, new_target, spare) else {
        regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
        return;
    };
    let [link_a, link_b] = spare;
    let mut args = args_of(regs);
    args[0] = new_target.as_ptr() as usize;
    match translate(translator, link, host_link, link_a, link_b) {
        Ok(Translated::Host(_)) => args[2] = host_link.as_ptr() as usize,
        Ok(_) => {}
        Err(errno) => {
            regs[0] = sys::neg_errno(errno);
            return;
        }
    }
    regs[0] = issue(nr::SYMLINKAT, &args);
}

/// `getcwd(buf, size)`: the kernel answers with the host path.
#[inline(never)]
fn getcwd(translator: &Translator<'static>, regs: &mut [u64; 31], scratch: &mut Scratch) {
    let buffer = regs[0] as *mut u8;
    let size = regs[1] as usize;
    let [host, guest, ..] = &mut scratch.paths;
    let len = match sys::getcwd(host) {
        Ok(len) => len,
        Err(errno) => {
            regs[0] = sys::neg_errno(errno);
            return;
        }
    };
    let Ok(guest_len) = translator.host_to_guest(&host[..len], guest) else {
        regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
        return;
    };
    // The kernel reports a cwd outside the process root as unreachable;
    // a host path under neither the root nor a binding (after a chroot,
    // or spawned there by the host) is exactly that.
    let mapped = translator.bindings.match_host(&host[..len]).is_some()
        || strip_prefix(translator.rootfs, &host[..len]).is_some();
    let guest_len = if !mapped {
        let mut writer = Writer::new(&mut guest[..]);
        if writer.push(b"(unreachable)").is_err() || writer.push(&host[..len]).is_err() {
            regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
            return;
        }
        writer.finish()
    } else {
        guest_len
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
fn serve_auxv(
    who: ProcExe,
    number: u32,
    regs: &[u64; 31],
    flags: usize,
    remote: &mut [u8; MAX_AUXV_BYTES],
) -> u64 {
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
        match remote_auxv(pid, remote) {
            Some(length) => &remote[..length],
            None => return issue(number, &args_of(regs)),
        }
    };
    serve_bytes(c"auxv", bytes, flags)
}

/// A read-only descriptor on `bytes`: a memfd positioned at the start,
/// close-on-exec unless the open asked otherwise. Write access is EACCES
/// the way procfs answers it.
pub(crate) fn serve_bytes(name: &core::ffi::CStr, bytes: &[u8], flags: usize) -> u64 {
    if flags & sys::O_ACCMODE != sys::O_RDONLY {
        return sys::neg_errno(sys::EACCES);
    }
    let fd = match sys::memfd_create(name.as_ptr().cast(), sys::MFD_CLOEXEC) {
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
/// A task that already has a tracer gets the kernel's EPERM at once:
/// told it succeeded, the child of an `strace` running under another
/// `strace -f` stops for the outer tracer while its own parent waits
/// forever for a stop that only a tracer is shown.
fn traceme(regs: &mut [u64; 31]) {
    if regs[0] == ptrace_traps::TRACEME {
        regs[0] = if traced() {
            sys::neg_errno(sys::EPERM)
        } else {
            TRACEME_REQUESTED.store(true, Ordering::Relaxed);
            0
        };
    } else {
        regs[0] = issue(nr::PTRACE, &args_of(regs));
    }
}

/// Whether a tracer is attached to the calling thread, read from
/// `/proc/thread-self/status`: ptrace state is per thread and the
/// request applies to the caller.
fn traced() -> bool {
    let Ok(fd) = sys::openat(
        sys::AT_FDCWD,
        b"/proc/thread-self/status\0".as_ptr(),
        sys::O_RDONLY | sys::O_CLOEXEC,
        0,
    ) else {
        return false;
    };
    let mut buffer = [0u8; 1024];
    let mut filled = 0;
    while filled < buffer.len() {
        match sys::read(fd, &mut buffer[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => filled += count,
        }
    }
    if let Err(_errno) = sys::close(fd) {}
    procfs::tracer_pid(&buffer[..filled]).unwrap_or(0) != 0
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
            log_traceme_refused(errno);
        }
    }
    regs[0] = issue(number, &args_of(regs));
}

#[inline(never)]
fn log_traceme_refused(errno: i32) {
    let mut line = log::Line::new();
    line.push(b"zdload: ptrace(PTRACE_TRACEME) before a signal to self failed: errno ");
    line.push_u64(errno as u64);
    line.emit();
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
