//! Raw arm64 Linux syscalls. Every `svc` the loader issues lives in this
//! image, which is why the seccomp filter can allow by instruction
//! pointer; the one deliberate exception is the self-test's stub copy.

use core::arch::asm;

pub use zd_engine_core::sysno::nr;

pub const AT_FDCWD: isize = -100;

pub const O_RDONLY: usize = 0;
pub const O_WRONLY: usize = 1;
pub const O_CREAT: usize = 0o100;
pub const O_APPEND: usize = 0o2000;
pub const O_CLOEXEC: usize = 0o2000000;
pub const O_ACCMODE: usize = 0o3;

pub const F_SETFD: usize = 2;
pub const MFD_CLOEXEC: usize = 1;
pub const SEEK_SET: usize = 0;

pub const PROT_NONE: usize = 0;
pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const PROT_EXEC: usize = 4;

pub const MAP_PRIVATE: usize = 0x02;
pub const MAP_FIXED: usize = 0x10;
pub const MAP_ANONYMOUS: usize = 0x20;

pub const SIGSYS: usize = 31;
pub const SIG_UNBLOCK: usize = 1;
pub const SA_SIGINFO: u64 = 0x4;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;

pub const PR_SET_NAME: usize = 15;
pub const PR_SET_SECCOMP: usize = 22;
pub const PR_SET_NO_NEW_PRIVS: usize = 38;
pub const PR_GET_NO_NEW_PRIVS: usize = 39;
pub const SECCOMP_MODE_FILTER: usize = 2;
pub const SECCOMP_SET_MODE_FILTER: usize = 1;
pub const SECCOMP_FILTER_FLAG_TSYNC: usize = 1;

pub const EACCES: i32 = 13;
pub const EFAULT: i32 = 14;
pub const EINVAL: i32 = 22;
pub const ENAMETOOLONG: i32 = 36;
pub const ENOSYS: i32 = 38;

#[inline(always)]
pub unsafe fn syscall6(
    number: u32,
    a0: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "svc #0",
            inlateout("x0") a0 as isize => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
            in("x8") number as usize,
            options(nostack),
        );
    }
    ret
}

/// Kernel return convention: `-4095..=-1` is `-errno`.
pub fn check(ret: isize) -> Result<usize, i32> {
    if ret < 0 && ret > -4096 {
        Err((-ret) as i32)
    } else {
        Ok(ret as usize)
    }
}

/// Encode `-errno` the way the kernel leaves it in `x0`.
pub fn neg_errno(errno: i32) -> u64 {
    (-(errno as i64)) as u64
}

pub fn write(fd: i32, bytes: &[u8]) -> isize {
    unsafe { syscall6(nr::WRITE, fd as usize, bytes.as_ptr() as usize, bytes.len(), 0, 0, 0) }
}

pub fn pread64(fd: i32, buffer: &mut [u8], offset: u64) -> Result<usize, i32> {
    check(unsafe {
        syscall6(
            nr::PREAD64,
            fd as usize,
            buffer.as_mut_ptr() as usize,
            buffer.len(),
            offset as usize,
            0,
            0,
        )
    })
}

/// `path` must be NUL-terminated.
pub fn openat(dirfd: isize, path: *const u8, flags: usize, mode: usize) -> Result<i32, i32> {
    check(unsafe { syscall6(nr::OPENAT, dirfd as usize, path as usize, flags, mode, 0, 0) })
        .map(|fd| fd as i32)
}

pub fn close(fd: i32) -> Result<(), i32> {
    check(unsafe { syscall6(nr::CLOSE, fd as usize, 0, 0, 0, 0, 0) }).map(|_| ())
}

pub fn mmap(
    addr: usize,
    length: usize,
    prot: usize,
    flags: usize,
    fd: i32,
    offset: u64,
) -> Result<usize, i32> {
    check(unsafe {
        syscall6(
            nr::MMAP,
            addr,
            length,
            prot,
            flags,
            fd as isize as usize,
            offset as usize,
        )
    })
}

pub fn mprotect(addr: usize, length: usize, prot: usize) -> Result<(), i32> {
    check(unsafe { syscall6(nr::MPROTECT, addr, length, prot, 0, 0, 0) }).map(|_| ())
}

/// `struct sigaction` as the arm64 kernel reads it (`SA_RESTORER` is
/// defined for the architecture, so the restorer slot is present).
#[repr(C)]
pub struct KernelSigaction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

pub fn rt_sigaction(signal: usize, action: &KernelSigaction) -> Result<(), i32> {
    check(unsafe {
        syscall6(
            nr::RT_SIGACTION,
            signal,
            action as *const KernelSigaction as usize,
            0,
            8,
            0,
            0,
        )
    })
    .map(|_| ())
}

pub fn rt_sigprocmask(how: usize, set: &u64) -> Result<(), i32> {
    check(unsafe { syscall6(nr::RT_SIGPROCMASK, how, set as *const u64 as usize, 0, 8, 0, 0) })
        .map(|_| ())
}

pub fn prctl(option: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> Result<usize, i32> {
    check(unsafe { syscall6(nr::PRCTL, option, a1, a2, a3, a4, 0) })
}

pub fn seccomp(operation: usize, flags: usize, args: usize) -> Result<usize, i32> {
    check(unsafe { syscall6(nr::SECCOMP, operation, flags, args, 0, 0, 0) })
}

pub fn memfd_create(name: *const u8, flags: usize) -> Result<i32, i32> {
    check(unsafe { syscall6(nr::MEMFD_CREATE, name as usize, flags, 0, 0, 0, 0) }).map(|fd| fd as i32)
}

pub fn lseek(fd: i32, offset: usize, whence: usize) -> Result<usize, i32> {
    check(unsafe { syscall6(nr::LSEEK, fd as usize, offset, whence, 0, 0, 0) })
}

pub fn fcntl(fd: i32, command: usize, argument: usize) -> Result<usize, i32> {
    check(unsafe { syscall6(nr::FCNTL, fd as usize, command, argument, 0, 0, 0) })
}

pub fn getpid() -> u32 {
    unsafe { syscall6(nr::GETPID, 0, 0, 0, 0, 0, 0) as u32 }
}

/// `getpid` carrying the probe magic in x0; the engine filter traps it
/// and the handler answers with the ack instead of a pid.
pub fn getpid_probe(magic: u64) -> u64 {
    unsafe { syscall6(nr::GETPID, magic as usize, 0, 0, 0, 0, 0) as u64 }
}

pub fn readlinkat(dirfd: isize, path: *const u8, buffer: &mut [u8]) -> Result<usize, i32> {
    check(unsafe {
        syscall6(
            nr::READLINKAT,
            dirfd as usize,
            path as usize,
            buffer.as_mut_ptr() as usize,
            buffer.len(),
            0,
            0,
        )
    })
}

pub fn exit_group(code: usize) -> ! {
    unsafe {
        asm!(
            "svc #0",
            in("x8") nr::EXIT_GROUP as usize,
            in("x0") code,
            options(noreturn, nostack),
        );
    }
}
