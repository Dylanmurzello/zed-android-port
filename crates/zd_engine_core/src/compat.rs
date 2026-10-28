//! Answers for syscalls trapped by Android's own per-app filter. Those
//! traps are untagged; the engine never re-issues them (the filter would
//! trap again) and instead returns what a guest can live with.

use crate::sysno::nr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// Return `-errno`.
    Errno(i32),
    /// Return 0: the operation is pretended to have succeeded.
    Success,
    /// `chroot`: succeeds only when the target is the guest root; the
    /// caller decides after translating the path.
    Chroot,
    /// `accept`: bionic only ever makes `accept4`, so the app allowlist lacks the
    /// legacy number glibc uses; the caller re-issues it as `accept4` with no flags.
    Accept4,
    /// Not in the table: `-ENOSYS` plus a one-time log line.
    Unknown,
}

pub const ENOSYS: i32 = 38;
pub const EPERM: i32 = 1;

/// Syscalls glibc probes at startup or falls back from on ENOSYS.
const NOT_IMPLEMENTED: &[u32] = &[
    nr::SET_ROBUST_LIST,
    nr::RSEQ,
    nr::FACCESSAT2,
    nr::OPENAT2,
    nr::FUTEX_WAITV,
    nr::FCHMODAT2,
    nr::IO_URING_SETUP,
    nr::IO_URING_ENTER,
    nr::IO_URING_REGISTER,
    nr::USERFAULTFD,
];

/// Privileged operations a real unprivileged process would also be
/// denied; `-EPERM` keeps tools on their documented fallback paths.
const DENIED: &[u32] = &[
    nr::MOUNT,
    nr::UMOUNT2,
    nr::PIVOT_ROOT,
    nr::SWAPON,
    nr::SWAPOFF,
    nr::ACCT,
    nr::REBOOT,
    nr::SETHOSTNAME,
    nr::SETDOMAINNAME,
    nr::INIT_MODULE,
    nr::FINIT_MODULE,
    nr::CLOCK_SETTIME,
    nr::SETTIMEOFDAY,
    nr::ADJTIMEX,
    nr::CLOCK_ADJTIME,
    nr::FSOPEN,
    nr::FSCONFIG,
    nr::FSMOUNT,
    nr::FSPICK,
    nr::OPEN_TREE,
    nr::MOVE_MOUNT,
    nr::MOUNT_SETATTR,
    nr::SETNS,
    nr::UNSHARE,
];

/// Identity changes: under fake root they succeed (everyone is root and
/// stays root); otherwise they are refused like for any unprivileged
/// process.
const SET_IDENTITY: &[u32] = &[
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

pub fn response(number: u32, fake_root: bool) -> Response {
    if NOT_IMPLEMENTED.contains(&number) {
        return Response::Errno(ENOSYS);
    }
    if DENIED.contains(&number) {
        return Response::Errno(EPERM);
    }
    if SET_IDENTITY.contains(&number) {
        return if fake_root {
            Response::Success
        } else {
            Response::Errno(EPERM)
        };
    }
    if number == nr::CHROOT {
        return Response::Chroot;
    }
    if number == nr::ACCEPT {
        return Response::Accept4;
    }
    Response::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_accept_becomes_accept4() {
        assert_eq!(response(nr::ACCEPT, false), Response::Accept4);
    }

    #[test]
    fn startup_probes_get_enosys() {
        assert_eq!(response(nr::SET_ROBUST_LIST, true), Response::Errno(ENOSYS));
        assert_eq!(response(nr::RSEQ, false), Response::Errno(ENOSYS));
        assert_eq!(response(nr::FACCESSAT2, true), Response::Errno(ENOSYS));
    }

    #[test]
    fn identity_depends_on_fake_root() {
        assert_eq!(response(nr::SETUID, true), Response::Success);
        assert_eq!(response(nr::SETUID, false), Response::Errno(EPERM));
        assert_eq!(response(nr::SETGROUPS, true), Response::Success);
    }

    #[test]
    fn mounts_are_denied_and_unknowns_flagged() {
        assert_eq!(response(nr::MOUNT, true), Response::Errno(EPERM));
        assert_eq!(response(nr::CHROOT, true), Response::Chroot);
        assert_eq!(response(9999, true), Response::Unknown);
    }
}
