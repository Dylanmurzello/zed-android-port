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
pub mod path;
pub mod sysno;
pub mod translate;

/// File name the loader ships under in the APK's native library dir.
/// It has to look like a shared library for the installer to extract it.
pub const LOADER_FILE_NAME: &str = "libzdload.so";

/// `argv[0]` basenames the loader treats as itself. With any other
/// basename the loader is being run through a symlink farm entry and
/// that basename is the guest program to run.
pub const LOADER_SELF_NAMES: &[&str] = &["libzdload.so", "zd-exec"];

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
    /// Guest-only environment overrides as `KEY=VALUE;KEY=VALUE`.
    pub const GUEST_ENV: &str = "ZD_GUEST_ENV";
}

/// Separator between entries in [`env::BINDS`] and [`env::GUEST_ENV`].
pub const LIST_SEPARATOR: char = ';';

/// Separator between the guest and host halves of a binding.
pub const BIND_SEPARATOR: char = ':';

/// Guest `PATH` used when the guest environment does not set one.
pub const DEFAULT_GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
