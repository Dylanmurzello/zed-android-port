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
    pub const IOCTL: u32 = 29;
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
    pub const FCHMOD: u32 = 52;
    pub const FCHMODAT: u32 = 53;
    pub const FCHOWNAT: u32 = 54;
    pub const FCHOWN: u32 = 55;
    pub const OPENAT: u32 = 56;
    pub const CLOSE: u32 = 57;
    pub const LSEEK: u32 = 62;
    pub const READ: u32 = 63;
    pub const WRITE: u32 = 64;
    pub const PREAD64: u32 = 67;
    pub const SENDFILE: u32 = 71;
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
    pub const PTRACE: u32 = 117;
    pub const KILL: u32 = 129;
    pub const TKILL: u32 = 130;
    pub const TGKILL: u32 = 131;
    pub const RT_SIGACTION: u32 = 134;
    pub const RT_SIGPROCMASK: u32 = 135;
    pub const RT_SIGQUEUEINFO: u32 = 138;
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
    pub const GETPGID: u32 = 155;
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
    pub const GETTID: u32 = 178;
    pub const SOCKET: u32 = 198;
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
    pub const RT_TGSIGQUEUEINFO: u32 = 240;
    pub const NAME_TO_HANDLE_AT: u32 = 264;
    pub const CLOCK_ADJTIME: u32 = 266;
    pub const SETNS: u32 = 268;
    pub const PROCESS_VM_READV: u32 = 270;
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
    pub const CLONE3: u32 = 435;
    pub const CLOSE_RANGE: u32 = 436;
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
    nr::FACCESSAT2,
    nr::OPENAT2,
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
    // a signal to self while a PTRACE_TRACEME is pending
    nr::KILL,
    nr::TKILL,
    nr::TGKILL,
    nr::RT_SIGQUEUEINFO,
    nr::RT_TGSIGQUEUEINFO,
];

/// Additional traps when presenting uid 0 to the guest. The identity
/// setters are allowlisted by Android's filter (bionic wraps them), so
/// the kernel would run them and refuse; trapped here they succeed and
/// nothing changes, which is what a root-only tool expects.
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
    nr::SETUID,
    nr::SETGID,
    nr::SETREUID,
    nr::SETREGID,
    nr::SETRESUID,
    nr::SETRESGID,
    nr::SETFSUID,
    nr::SETFSGID,
    nr::SETGROUPS,
    nr::CAPSET,
];

/// `ioctl` requests trapped by argument. glibc 2.41+ implements
/// `tcgetattr`/`tcsetattr` (and so `isatty`, readline's raw mode, every
/// interactive shell's startup check) with the termios2 requests, and
/// Android's SELinux policy grants app domains only the classic tty
/// ioctls on `devpts`, so these four fail with EACCES unless the engine
/// serves them through `TCGETS`/`TCSETS*`.
pub mod ioctl {
    pub const TCGETS: u64 = 0x5401;
    pub const TCSETS: u64 = 0x5402;
    pub const TCSETSW: u64 = 0x5403;
    pub const TCSETSF: u64 = 0x5404;
    pub const TCGETS2: u64 = 0x802c_542a;
    pub const TCSETS2: u64 = 0x402c_542b;
    pub const TCSETSW2: u64 = 0x402c_542c;
    pub const TCSETSF2: u64 = 0x402c_542d;
    pub const TERMIOS2_REQUESTS: &[u64] = &[TCGETS2, TCSETS2, TCSETSW2, TCSETSF2];
}

/// `socket` calls trapped by address family. Android 16 lets apps create
/// netlink sockets only for routing; the other families fail with EACCES,
/// which libaudit users (shadow's useradd, groupadd, passwd) treat as
/// fatal where a kernel without the family (EPROTONOSUPPORT) is skipped.
pub mod socket {
    pub const AF_NETLINK: u64 = 16;
    pub const TRAPPED_FAMILIES: &[u64] = &[AF_NETLINK];
}

/// `clone3` is trapped unless its size argument is the VER0 struct size.
/// glibc's posix_spawn passes `CLONE_CLEAR_SIGHAND`, which would reset
/// the engine's own SIGSYS handler in the vfork child; the handler clears
/// the flag in the guest's struct and re-executes the call in the VER0
/// form, which the filter lets through.
/// `ptrace(2)` requests the filter traps.
pub mod ptrace {
    /// Answered by the handler and performed later: by the loader after the
    /// next exec, or by the handler before a signal the guest sends to
    /// itself; see `env::TRACEME`.
    pub const TRACEME: u64 = 0;
    pub const TRAPPED_REQUESTS: &[u64] = &[TRACEME];
}

pub mod clone {
    pub const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
    pub const CLONE_ARGS_SIZE_VER0: u64 = 64;
    pub const PASSED_SIZES: &[u64] = &[CLONE_ARGS_SIZE_VER0];
}

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

/// Linux name of a syscall the engine knows, for log lines.
pub fn name(number: u32) -> Option<&'static str> {
    Some(match number {
        nr::SETXATTR => "setxattr",
        nr::LSETXATTR => "lsetxattr",
        nr::GETXATTR => "getxattr",
        nr::LGETXATTR => "lgetxattr",
        nr::LISTXATTR => "listxattr",
        nr::LLISTXATTR => "llistxattr",
        nr::REMOVEXATTR => "removexattr",
        nr::LREMOVEXATTR => "lremovexattr",
        nr::GETCWD => "getcwd",
        nr::FCNTL => "fcntl",
        nr::INOTIFY_ADD_WATCH => "inotify_add_watch",
        nr::IOCTL => "ioctl",
        nr::MKNODAT => "mknodat",
        nr::MKDIRAT => "mkdirat",
        nr::UNLINKAT => "unlinkat",
        nr::SYMLINKAT => "symlinkat",
        nr::LINKAT => "linkat",
        nr::RENAMEAT => "renameat",
        nr::UMOUNT2 => "umount2",
        nr::MOUNT => "mount",
        nr::PIVOT_ROOT => "pivot_root",
        nr::STATFS => "statfs",
        nr::TRUNCATE => "truncate",
        nr::FACCESSAT => "faccessat",
        nr::CHDIR => "chdir",
        nr::CHROOT => "chroot",
        nr::FCHMOD => "fchmod",
        nr::FCHMODAT => "fchmodat",
        nr::FCHOWNAT => "fchownat",
        nr::FCHOWN => "fchown",
        nr::OPENAT => "openat",
        nr::CLOSE => "close",
        nr::LSEEK => "lseek",
        nr::READ => "read",
        nr::WRITE => "write",
        nr::PREAD64 => "pread64",
        nr::SENDFILE => "sendfile",
        nr::READLINKAT => "readlinkat",
        nr::NEWFSTATAT => "newfstatat",
        nr::FSTAT => "fstat",
        nr::UTIMENSAT => "utimensat",
        nr::ACCT => "acct",
        nr::CAPGET => "capget",
        nr::CAPSET => "capset",
        nr::EXIT => "exit",
        nr::EXIT_GROUP => "exit_group",
        nr::UNSHARE => "unshare",
        nr::SET_ROBUST_LIST => "set_robust_list",
        nr::INIT_MODULE => "init_module",
        nr::CLOCK_SETTIME => "clock_settime",
        nr::CLOCK_GETTIME => "clock_gettime",
        nr::RT_SIGACTION => "rt_sigaction",
        nr::RT_SIGPROCMASK => "rt_sigprocmask",
        nr::RT_SIGRETURN => "rt_sigreturn",
        nr::REBOOT => "reboot",
        nr::SETREGID => "setregid",
        nr::SETGID => "setgid",
        nr::SETREUID => "setreuid",
        nr::SETUID => "setuid",
        nr::SETRESUID => "setresuid",
        nr::GETRESUID => "getresuid",
        nr::SETRESGID => "setresgid",
        nr::GETRESGID => "getresgid",
        nr::SETFSUID => "setfsuid",
        nr::SETFSGID => "setfsgid",
        nr::GETGROUPS => "getgroups",
        nr::SETGROUPS => "setgroups",
        nr::UNAME => "uname",
        nr::SETHOSTNAME => "sethostname",
        nr::SETDOMAINNAME => "setdomainname",
        nr::PRCTL => "prctl",
        nr::SETTIMEOFDAY => "settimeofday",
        nr::ADJTIMEX => "adjtimex",
        nr::GETPID => "getpid",
        nr::GETTID => "gettid",
        nr::TGKILL => "tgkill",
        nr::KILL => "kill",
        nr::TKILL => "tkill",
        nr::RT_SIGQUEUEINFO => "rt_sigqueueinfo",
        nr::RT_TGSIGQUEUEINFO => "rt_tgsigqueueinfo",
        nr::PTRACE => "ptrace",
        nr::GETUID => "getuid",
        nr::GETEUID => "geteuid",
        nr::GETGID => "getgid",
        nr::GETEGID => "getegid",
        nr::SOCKET => "socket",
        nr::BIND => "bind",
        nr::CONNECT => "connect",
        nr::SENDTO => "sendto",
        nr::RECVFROM => "recvfrom",
        nr::MUNMAP => "munmap",
        nr::CLONE => "clone",
        nr::EXECVE => "execve",
        nr::MMAP => "mmap",
        nr::SWAPON => "swapon",
        nr::SWAPOFF => "swapoff",
        nr::MPROTECT => "mprotect",
        nr::NAME_TO_HANDLE_AT => "name_to_handle_at",
        nr::CLOCK_ADJTIME => "clock_adjtime",
        nr::SETNS => "setns",
        nr::PROCESS_VM_READV => "process_vm_readv",
        nr::FINIT_MODULE => "finit_module",
        nr::RENAMEAT2 => "renameat2",
        nr::SECCOMP => "seccomp",
        nr::MEMFD_CREATE => "memfd_create",
        nr::EXECVEAT => "execveat",
        nr::USERFAULTFD => "userfaultfd",
        nr::STATX => "statx",
        nr::RSEQ => "rseq",
        nr::IO_URING_SETUP => "io_uring_setup",
        nr::IO_URING_ENTER => "io_uring_enter",
        nr::IO_URING_REGISTER => "io_uring_register",
        nr::OPEN_TREE => "open_tree",
        nr::MOVE_MOUNT => "move_mount",
        nr::FSOPEN => "fsopen",
        nr::FSCONFIG => "fsconfig",
        nr::FSMOUNT => "fsmount",
        nr::FSPICK => "fspick",
        nr::CLOSE_RANGE => "close_range",
        nr::OPENAT2 => "openat2",
        nr::FACCESSAT2 => "faccessat2",
        nr::MOUNT_SETATTR => "mount_setattr",
        nr::FUTEX_WAITV => "futex_waitv",
        nr::FCHMODAT2 => "fchmodat2",
        nr::CLONE3 => "clone3",
        _ => return None,
    })
}

/// `getpid()` with this value in x0 is the filter-presence probe: the
/// filter traps it with [`TAG_PROBE`] and the handler returns
/// [`PROBE_ACK`]. A real `getpid()` never carries an argument, so the
/// kernel path is unaffected when no filter is installed.
pub const PROBE_MAGIC: u64 = 0x5A44_4C4F_4144_0001;
pub const PROBE_ACK: u64 = 0x5A44_0001;

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn every_trapped_syscall_has_a_name() {
        for number in TRAP_TABLE.iter().chain(TRAP_TABLE_FAKE_ROOT) {
            assert!(name(*number).is_some(), "syscall {number} has no name");
        }
        assert_eq!(name(nr::LINKAT), Some("linkat"));
        assert_eq!(name(9999), None);
    }
}
