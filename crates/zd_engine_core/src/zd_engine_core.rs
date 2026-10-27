//! Shared definitions for the Zdroid rootfs engine.
//!
//! Two binaries build on this crate: `zd_loader`, the static libc-free
//! loader that the kernel execs for every guest process, and the
//! supervisor in `zdroid_runtime`, which installs the rootfs and spawns
//! the loader. Everything here is `no_std` and allocation-free so the
//! loader can link it without a libc.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod bindings;
pub mod bpf;
pub mod compat;
pub mod elf;
pub mod identity;
pub mod path;
pub mod shebang;
pub mod stack;
pub mod sysno;
pub mod translate;

/// File name the loader ships under in the APK's native library dir.
/// It has to look like a shared library for the installer to extract it.
pub const LOADER_FILE_NAME: &str = "libzdload.so";

/// `argv[0]` basenames the loader treats as itself: the file in the
/// APK, the stable `<data>/files/bin/zdload` symlink the supervisor
/// execs through, and the `zd-exec` name the farm inherits. With any
/// other basename the loader is being run through a symlink farm entry
/// and that basename is the guest program to run.
pub const LOADER_SELF_NAMES: &[&str] = &["libzdload.so", "zdload", "zd-exec"];

/// Name of the stable symlink under `<data>/files/bin` that points at
/// the current install's loader. The native library dir moves on every
/// install, so persistent state only ever names this link.
pub const LOADER_LINK_NAME: &str = "zdload";

/// Address the loader image is linked at. Chosen so the engine's BPF
/// instruction-pointer test is two 32-bit compares, and so the loader
/// never collides with kernel-placed mappings on 39-bit or 48-bit
/// address-space kernels.
pub const LOADER_IMAGE_BASE: u64 = 0x54_0000_0000;

/// Size of the fixed address window, starting at [`LOADER_IMAGE_BASE`],
/// from which the seccomp filter allows syscalls. It covers the whole
/// loader image rather than its exact text so a bigger loader after an
/// APK update still fits the window a long-running process inherited.
pub const LOADER_WINDOW_SIZE: u64 = 0x40_0000;

/// Environment variables the loader reads. They are passed through to
/// every descendant and re-injected on exec if a guest dropped them.
pub mod env {
    /// Host directory of the extracted rootfs.
    pub const ROOTFS: &str = "ZD_ROOTFS";
    /// Extra bindings as `guest:host;guest:host`; a bare `guest` entry
    /// maps the path to itself.
    pub const BINDS: &str = "ZD_BINDS";
    /// `1` to present uid 0 to the guest.
    pub const FAKE_ROOT: &str = "ZD_FAKE_ROOT";
    /// Optional file the loader appends diagnostics to.
    pub const LOG: &str = "ZD_LOG";
    /// Guest-only environment overrides as `KEY=VALUE;KEY=VALUE`, the
    /// editor's view of `HOME`, `PATH` and friends translated into the
    /// guest's at the boundary. Applied by the loader where present (the
    /// supervisor's spawns) and then removed, so a guest's own exports
    /// reach its children untouched. Per exec.
    pub const GUEST_ENV: &str = "ZD_GUEST_ENV";
    /// Guest path of the program to run when the loader is exec'd with
    /// the guest's own argv (so the kernel's argument area, and with it
    /// `/proc/<pid>/cmdline`, is exactly the guest's). Per exec: the
    /// loader removes it from the guest's environment.
    pub const PROGRAM: &str = "ZD_PROGRAM";
    /// `1` runs the self-test before loading the program. Per exec.
    pub const SELFTEST: &str = "ZD_SELFTEST";
    /// `1` closes every fd above 2 before the guest starts. The
    /// supervisor sets it on its own environment so spawns from the
    /// editor (whose graphics stack opens fds without CLOEXEC) do not
    /// leak them; guests exec their children without it, so fds they
    /// pass on purpose still arrive. Per exec.
    pub const CLOSE_FDS: &str = "ZD_CLOSE_FDS";
    /// The fake-root identity of the process being exec'd, as
    /// [`crate::identity::Identity::encode`] writes it. Per exec: the
    /// handler writes it from its own record on every `execve`, so a
    /// stale copy in a guest's environment never wins.
    pub const IDENTITY: &str = "ZD_IDENTITY";
    /// `1` when the process being exec'd had asked for `ptrace(PTRACE_TRACEME)`.
    /// The handler answers that request itself and carries it here; the
    /// loader attaches to the parent only once the guest is mapped and its
    /// vector recorded, then stops with the SIGTRAP a traced exec delivers.
    /// Attaching at the request would turn every SIGSYS between it and the
    /// exec into a ptrace stop that the parent, blocked on its exec pipe,
    /// never resumes (lldb-server, gdb), and a tracer reading the process at
    /// its first stop would see the loader's vector instead of the guest's.
    /// Per exec.
    pub const TRACEME: &str = "ZD_TRACEME";
    /// Variables that describe one exec and must not reach the guest.
    pub const PER_EXEC: &[&str] = &[PROGRAM, SELFTEST, CLOSE_FDS, IDENTITY, GUEST_ENV, TRACEME];
}

/// Separator between entries in [`env::BINDS`] and [`env::GUEST_ENV`].
pub const LIST_SEPARATOR: char = ';';

/// Separator between the guest and host halves of a binding.
pub const BIND_SEPARATOR: char = ':';

/// Guest `PATH` used when the guest environment does not set one. The
/// farm advertises binaries from `root/.cargo/bin` and `root/.local/bin`,
/// so the loader's name lookup for a farm link has to search them too.
pub const DEFAULT_GUEST_PATH: &str =
    "/root/.cargo/bin:/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
