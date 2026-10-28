//! The Zdroid runtime: a stock Ubuntu base image under `<data>/files/rootfs`,
//! every process exec'd through the engine loader (`libzdload.so`, reached
//! via the stable `<data>/files/bin/zdload` link).
//!
//! The loader maps the guest binary itself and translates path syscalls
//! into the rootfs, so nothing in the image is patched. This crate installs
//! the image from the APK's bundled copy and shapes the environment the
//! editor and its children run with.
//!
//! - [`runtime`]: the [`Runtime`] handle (paths, environment, health).
//! - [`install`]: extraction of the bundled image, seeds, the optional
//!   essentials step.
//! - [`env`]: the baseline Zed-process environment.
//! - [`progress`]: [`ProgressSink`] for long operations, [`HealthStatus`].

use std::path::PathBuf;
use std::sync::OnceLock;

pub mod env;
pub mod install;
pub mod progress;
pub mod runtime;

pub use install::Image;
pub use progress::{HealthStatus, ProgressSink};
pub use runtime::{Runtime, TerminalShell};

/// App files dir when the platform did not hand us one.
pub const DEFAULT_DATA_PATH: &str = "/data/data/com.zdroid/files";

static DATA_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Record the app files dir once at boot so code without access to the
/// `AndroidApp` (actions, helpers) can find the runtime.
pub fn set_data_path(path: PathBuf) {
    if DATA_PATH.set(path).is_err() {
        log::debug!("zdroid_runtime: data_path already set; ignoring re-set");
    }
}

/// The app files dir recorded by [`set_data_path`], or the default.
pub fn data_path() -> PathBuf {
    DATA_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_PATH))
}

static NAMESERVERS: OnceLock<Vec<String>> = OnceLock::new();

/// Public resolvers used when the platform has not reported any.
pub const FALLBACK_NAMESERVERS: &[&str] = &["1.1.1.1", "8.8.8.8"];

/// Record the device's active DNS servers (from the platform bridge)
/// so the installer can seed the guest's `/etc/resolv.conf`.
pub fn set_nameservers(servers: Vec<String>) {
    if NAMESERVERS.set(servers).is_err() {
        log::debug!("zdroid_runtime: nameservers already set; ignoring re-set");
    }
}

/// The recorded nameservers, or the public fallback. Never empty.
pub fn nameservers() -> Vec<String> {
    match NAMESERVERS.get() {
        Some(servers) if !servers.is_empty() => servers.clone(),
        _ => FALLBACK_NAMESERVERS.iter().map(|s| s.to_string()).collect(),
    }
}
