//! arm64 syscall numbers the engine traps or issues, the seccomp trap
//! tags, and the filter-presence probe.

/// Syscall numbers from the arm64 (asm-generic) table.
pub mod nr {
    pub const GETCWD: u32 = 17;
    pub const FACCESSAT: u32 = 48;
    pub const CHDIR: u32 = 49;
    pub const OPENAT: u32 = 56;
    pub const CLOSE: u32 = 57;
    pub const READ: u32 = 63;
    pub const WRITE: u32 = 64;
    pub const READLINKAT: u32 = 78;
    pub const NEWFSTATAT: u32 = 79;
    pub const EXIT: u32 = 93;
    pub const EXIT_GROUP: u32 = 94;
    pub const SET_ROBUST_LIST: u32 = 99;
    pub const RT_SIGACTION: u32 = 134;
    pub const RT_SIGPROCMASK: u32 = 135;
    pub const RT_SIGRETURN: u32 = 139;
    pub const PRCTL: u32 = 167;
    pub const GETPID: u32 = 172;
    pub const MUNMAP: u32 = 215;
    pub const CLONE: u32 = 220;
    pub const EXECVE: u32 = 221;
    pub const MMAP: u32 = 222;
    pub const MPROTECT: u32 = 226;
    pub const SECCOMP: u32 = 277;
    pub const STATX: u32 = 291;
    pub const RSEQ: u32 = 293;
    pub const OPENAT2: u32 = 437;
    pub const FACCESSAT2: u32 = 439;
}

/// Path syscalls trapped in chunk 1: enough for glibc's `ld.so` and a
/// coreutils binary to find their files.
pub const TRAP_TABLE_CHUNK1: &[u32] = &[
    nr::OPENAT,
    nr::NEWFSTATAT,
    nr::STATX,
    nr::FACCESSAT,
    nr::READLINKAT,
];

/// `SECCOMP_RET_DATA` values carried in `si_errno` of our SIGSYS. The
/// high byte identifies the engine; Android's own filter uses data 0.
pub const TAG_BASE: u16 = 0x5A00;
pub const TAG_PATH: u16 = 0x5A01;
pub const TAG_PROBE: u16 = 0x5A02;

pub fn is_engine_tag(data: u16) -> bool {
    data & 0xFF00 == TAG_BASE
}

/// `getpid()` with this value in x0 is the filter-presence probe: the
/// filter traps it with [`TAG_PROBE`] and the handler returns
/// [`PROBE_ACK`]. A real `getpid()` never carries an argument, so the
/// kernel path is unaffected when no filter is installed.
pub const PROBE_MAGIC: u64 = 0x5A44_4C4F_4144_0001;
pub const PROBE_ACK: u64 = 0x5A44_0001;
