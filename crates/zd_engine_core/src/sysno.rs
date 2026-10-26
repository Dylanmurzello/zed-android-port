//! arm64 syscall numbers the engine traps or issues, the seccomp trap
//! tags, and the filter-presence probe.

/// Syscall numbers from the arm64 (asm-generic) table.
pub mod nr {
    pub const SETXATTR: u32 = 5;
    pub const LSETXATTR: u32 = 6;
    pub const GETXATTR: u32 = 8;
    pub const LGETXATTR: u32 = 9;
    pub const LISTXATTR: u32 = 11;
    pub const LLISTXATTR: u32 = 12;
    pub const REMOVEXATTR: u32 = 14;
    pub const LREMOVEXATTR: u32 = 15;
    pub const GETCWD: u32 = 17;
    pub const FCNTL: u32 = 25;
    pub const INOTIFY_ADD_WATCH: u32 = 27;
    pub const MKNODAT: u32 = 33;
    pub const MKDIRAT: u32 = 34;
    pub const UNLINKAT: u32 = 35;
    pub const SYMLINKAT: u32 = 36;
    pub const LINKAT: u32 = 37;
    pub const RENAMEAT: u32 = 38;
    pub const UMOUNT2: u32 = 39;
    pub const MOUNT: u32 = 40;
    pub const PIVOT_ROOT: u32 = 41;
    pub const STATFS: u32 = 43;
    pub const TRUNCATE: u32 = 45;
    pub const FACCESSAT: u32 = 48;
    pub const CHDIR: u32 = 49;
    pub const CHROOT: u32 = 51;
    pub const FCHMODAT: u32 = 53;
    pub const FCHOWNAT: u32 = 54;
    pub const FCHOWN: u32 = 55;
    pub const OPENAT: u32 = 56;
    pub const CLOSE: u32 = 57;
    pub const LSEEK: u32 = 62;
    pub const READ: u32 = 63;
    pub const WRITE: u32 = 64;
    pub const PREAD64: u32 = 67;
    pub const READLINKAT: u32 = 78;
    pub const NEWFSTATAT: u32 = 79;
    pub const FSTAT: u32 = 80;
    pub const UTIMENSAT: u32 = 88;
    pub const ACCT: u32 = 89;
    pub const CAPGET: u32 = 90;
    pub const CAPSET: u32 = 91;
    pub const EXIT: u32 = 93;
    pub const EXIT_GROUP: u32 = 94;
    pub const UNSHARE: u32 = 97;
    pub const SET_ROBUST_LIST: u32 = 99;
    pub const INIT_MODULE: u32 = 105;
    pub const CLOCK_SETTIME: u32 = 112;
    pub const CLOCK_GETTIME: u32 = 113;
    pub const RT_SIGACTION: u32 = 134;
    pub const RT_SIGPROCMASK: u32 = 135;
    pub const RT_SIGRETURN: u32 = 139;
    pub const REBOOT: u32 = 142;
    pub const SETREGID: u32 = 143;
    pub const SETGID: u32 = 144;
    pub const SETREUID: u32 = 145;
    pub const SETUID: u32 = 146;
    pub const SETRESUID: u32 = 147;
    pub const GETRESUID: u32 = 148;
    pub const SETRESGID: u32 = 149;
    pub const GETRESGID: u32 = 150;
    pub const SETFSUID: u32 = 151;
    pub const SETFSGID: u32 = 152;
    pub const GETGROUPS: u32 = 158;
    pub const SETGROUPS: u32 = 159;
    pub const UNAME: u32 = 160;
    pub const SETHOSTNAME: u32 = 161;
    pub const SETDOMAINNAME: u32 = 162;
    pub const PRCTL: u32 = 167;
    pub const SETTIMEOFDAY: u32 = 170;
    pub const ADJTIMEX: u32 = 171;
    pub const GETPID: u32 = 172;
    pub const GETUID: u32 = 174;
    pub const GETEUID: u32 = 175;
    pub const GETGID: u32 = 176;
    pub const GETEGID: u32 = 177;
    pub const BIND: u32 = 200;
    pub const CONNECT: u32 = 203;
    pub const SENDTO: u32 = 206;
    pub const RECVFROM: u32 = 207;
    pub const MUNMAP: u32 = 215;
    pub const CLONE: u32 = 220;
    pub const EXECVE: u32 = 221;
    pub const MMAP: u32 = 222;
    pub const SWAPON: u32 = 224;
    pub const SWAPOFF: u32 = 225;
    pub const MPROTECT: u32 = 226;
    pub const NAME_TO_HANDLE_AT: u32 = 264;
    pub const CLOCK_ADJTIME: u32 = 266;
    pub const SETNS: u32 = 268;
    pub const FINIT_MODULE: u32 = 273;
    pub const RENAMEAT2: u32 = 276;
    pub const SECCOMP: u32 = 277;
    pub const MEMFD_CREATE: u32 = 279;
    pub const EXECVEAT: u32 = 281;
    pub const USERFAULTFD: u32 = 282;
    pub const STATX: u32 = 291;
    pub const RSEQ: u32 = 293;
    pub const IO_URING_SETUP: u32 = 425;
    pub const IO_URING_ENTER: u32 = 426;
    pub const IO_URING_REGISTER: u32 = 427;
    pub const OPEN_TREE: u32 = 428;
    pub const MOVE_MOUNT: u32 = 429;
    pub const FSOPEN: u32 = 430;
    pub const FSCONFIG: u32 = 431;
    pub const FSMOUNT: u32 = 432;
    pub const FSPICK: u32 = 433;
    pub const OPENAT2: u32 = 437;
    pub const FACCESSAT2: u32 = 439;
    pub const MOUNT_SETATTR: u32 = 442;
    pub const FUTEX_WAITV: u32 = 449;
    pub const FCHMODAT2: u32 = 452;
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

/// Syscalls with a path, a sockaddr, a signal set, or a cwd result that
/// the engine translates. Everything else passes through the filter.
pub const TRAP_TABLE: &[u32] = &[
    // path in x0
    nr::SETXATTR,
    nr::LSETXATTR,
    nr::GETXATTR,
    nr::LGETXATTR,
    nr::LISTXATTR,
    nr::LLISTXATTR,
    nr::REMOVEXATTR,
    nr::LREMOVEXATTR,
    nr::STATFS,
    nr::TRUNCATE,
    nr::CHDIR,
    nr::EXECVE,
    // path in x1
    nr::INOTIFY_ADD_WATCH,
    nr::MKNODAT,
    nr::MKDIRAT,
    nr::UNLINKAT,
    nr::FACCESSAT,
    nr::FCHMODAT,
    nr::FCHOWNAT,
    nr::OPENAT,
    nr::READLINKAT,
    nr::NEWFSTATAT,
    nr::UTIMENSAT,
    nr::NAME_TO_HANDLE_AT,
    nr::EXECVEAT,
    nr::STATX,
    // two paths, symlink target, cwd result
    nr::SYMLINKAT,
    nr::LINKAT,
    nr::RENAMEAT,
    nr::RENAMEAT2,
    nr::GETCWD,
    // sockaddr_un
    nr::BIND,
    nr::CONNECT,
    nr::SENDTO,
    // SIGSYS must never be blocked
    nr::RT_SIGACTION,
    nr::RT_SIGPROCMASK,
];

/// Additional traps when presenting uid 0 to the guest.
pub const TRAP_TABLE_FAKE_ROOT: &[u32] = &[
    nr::FSTAT,
    nr::FCHOWN,
    nr::GETUID,
    nr::GETEUID,
    nr::GETGID,
    nr::GETEGID,
    nr::GETRESUID,
    nr::GETRESGID,
    nr::GETGROUPS,
];

/// Upper bound on the trapped set, for the loader's stack array.
pub const MAX_TRAPPED: usize = TRAP_TABLE.len() + TRAP_TABLE_FAKE_ROOT.len();

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
