//! The SIGSYS trap handler and the seccomp filter that feeds it.
//!
//! The kernel delivers a tagged SIGSYS for every path syscall the guest
//! issues; the handler translates the path argument, re-issues the
//! syscall from loader text (which the filter allows by instruction
//! pointer) and writes the result into the saved `x0`. Untagged SIGSYS
//! come from Android's own per-app filter and are answered with
//! `-ENOSYS`, which glibc tolerates for every syscall in that set.

use core::mem::offset_of;
use core::sync::atomic::{AtomicU64, Ordering};

use zd_engine_core::bpf::{self, FilterSpec, SockFilter};
use zd_engine_core::path::MAX_PATH;
use zd_engine_core::sysno::{
    PROBE_ACK, PROBE_MAGIC, TAG_PATH, TAG_PROBE, TRAP_TABLE_CHUNK1, is_engine_tag, nr,
};
use zd_engine_core::translate::{ProcExe, Translated, Translator};
use zd_engine_core::{LOADER_IMAGE_BASE, LOADER_WINDOW_SIZE};

use crate::config;
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
    sys::rt_sigprocmask(sys::SIG_UNBLOCK, &(1u64 << (sys::SIGSYS - 1)))
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
    let program = bpf::build(&FilterSpec {
        trapped: TRAP_TABLE_CHUNK1,
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
        compat(number, regs);
        return;
    }
    dispatch(number, regs);
}

/// Android's filter trapped a syscall bionic never makes. Every one of
/// them has a userspace fallback in glibc when it sees `ENOSYS`.
fn compat(number: u32, regs: &mut [u64; 31]) {
    COMPAT_COUNT.fetch_add(1, Ordering::Relaxed);
    regs[0] = sys::neg_errno(sys::ENOSYS);
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
        log::append_to_log_file(line_bytes(&line));
    }
}

fn line_bytes(line: &log::Line) -> &[u8] {
    line.as_bytes()
}

fn dispatch(number: u32, regs: &mut [u64; 31]) {
    let Some(translator) = config::translator() else {
        regs[0] = sys::neg_errno(sys::ENOSYS);
        return;
    };
    match number {
        nr::OPENAT | nr::NEWFSTATAT | nr::FACCESSAT | nr::STATX | nr::READLINKAT => {
            path_syscall(translator, number, regs)
        }
        _ => regs[0] = sys::neg_errno(sys::ENOSYS),
    }
}

/// Bytes up to the NUL, or `None` when no NUL appears within `max`.
unsafe fn cstr_bounded<'a>(pointer: *const u8, max: usize) -> Option<&'a [u8]> {
    for len in 0..max {
        // SAFETY: the caller supplies a guest pointer that the guest
        // would have handed to the kernel; a bad one faults here instead.
        if unsafe { *pointer.add(len) } == 0 {
            return Some(unsafe { core::slice::from_raw_parts(pointer, len) });
        }
    }
    None
}

fn reissue(number: u32, regs: &[u64; 31], path: u64) -> u64 {
    unsafe {
        sys::syscall6(
            number,
            regs[0] as usize,
            path as usize,
            regs[2] as usize,
            regs[3] as usize,
            regs[4] as usize,
            regs[5] as usize,
        ) as u64
    }
}

fn is_self(who: ProcExe) -> bool {
    match who {
        ProcExe::SelfProcess | ProcExe::ThreadSelf => true,
        ProcExe::Pid(pid) => pid == sys::getpid(),
    }
}

/// Syscalls whose path lives in `x1` and whose other arguments pass
/// through unchanged.
fn path_syscall(translator: &Translator<'static>, number: u32, regs: &mut [u64; 31]) {
    let pointer = regs[1] as *const u8;
    if pointer.is_null() {
        regs[0] = sys::neg_errno(sys::EFAULT);
        return;
    }
    let Some(guest) = (unsafe { cstr_bounded(pointer, MAX_PATH) }) else {
        regs[0] = sys::neg_errno(sys::ENAMETOOLONG);
        return;
    };
    let mut host = [0u8; MAX_PATH];
    let result = match translator.guest_to_host(guest, &mut host) {
        Ok(Translated::Relative) => reissue(number, regs, regs[1]),
        Ok(Translated::Host(_)) => reissue(number, regs, host.as_ptr() as u64),
        Ok(Translated::ProcExe(who)) if is_self(who) => {
            let guest_exe = config::get().guest_exe.as_bytes();
            if number == nr::READLINKAT {
                regs[0] = copy_out(regs[2] as *mut u8, regs[3] as usize, guest_exe);
                return;
            }
            match translator.guest_to_host(guest_exe, &mut host) {
                Ok(Translated::Host(_)) => reissue(number, regs, host.as_ptr() as u64),
                _ => reissue(number, regs, config::get().guest_exe.as_ptr() as u64),
            }
        }
        Ok(Translated::ProcExe(_)) => reissue(number, regs, regs[1]),
        Err(_) => sys::neg_errno(sys::ENAMETOOLONG),
    };
    regs[0] = result;
    if number == nr::READLINKAT {
        detranslate_readlink(translator, regs, &mut host);
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
