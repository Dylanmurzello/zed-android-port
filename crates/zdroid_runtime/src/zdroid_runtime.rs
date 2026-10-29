//! The Zdroid runtime: a stock Ubuntu base image under `<data>/files/rootfs`,
//! every process exec'd through the engine loader (`libzdload.so`, reached
//! via the stable `<data>/files/bin/zdload` link).
//!
//! The loader maps the guest binary itself and translates path syscalls
//! into the rootfs, so nothing in the image is patched. This crate installs
//! the image from the APK's bundled copy and shapes the environment the
//! editor and its children run with.
//!
//! - [`runtime`]: the [`Runtime`] handle (paths, environment).
//! - [`install`]: extraction of the bundled image, seeds, the optional
//!   essentials step.
//! - [`env`]: the baseline Zed-process environment.
//! - [`progress`]: [`ProgressSink`] for long operations.

use std::path::PathBuf;
use std::sync::OnceLock;

pub mod env;
pub mod install;
pub mod progress;
pub mod runtime;

pub use env::EnvOp;
pub use install::Image;
pub use progress::ProgressSink;
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

static TIMEZONE: OnceLock<Option<String>> = OnceLock::new();

/// The zone used when the platform reports none.
pub const FALLBACK_TIMEZONE: &str = "Etc/UTC";

/// Record the device's IANA time zone (`persist.sys.timezone`) so the
/// installer can seed the guest's `/etc/localtime` and `/etc/timezone`.
/// A name that is not a plain zone id is dropped.
pub fn set_timezone(zone: Option<String>) {
    let zone = zone.filter(|zone| is_zone_id(zone));
    if TIMEZONE.set(zone).is_err() {
        log::debug!("zdroid_runtime: timezone already set; ignoring re-set");
    }
}

/// The recorded zone, or UTC.
pub fn timezone() -> String {
    match TIMEZONE.get() {
        Some(Some(zone)) => zone.clone(),
        _ => FALLBACK_TIMEZONE.to_string(),
    }
}

/// An IANA zone id as tzdata files it: `Area/Location` components of
/// letters, digits, `_`, `-` and `+`, nothing that walks the tree.
pub fn is_zone_id(zone: &str) -> bool {
    !zone.is_empty()
        && zone.len() <= 64
        && zone.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-+".contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    use super::is_zone_id;

    #[test]
    fn zone_ids_are_plain_paths_under_zoneinfo() {
        for zone in [
            "America/New_York",
            "Etc/UTC",
            "UTC",
            "Etc/GMT+5",
            "America/Argentina/Buenos_Aires",
        ] {
            assert!(is_zone_id(zone), "{zone}");
        }
        for zone in [
            "",
            "/etc/passwd",
            "../../etc",
            "America/../..",
            "a b",
            "Europe/",
            "zone\n",
        ] {
            assert!(!is_zone_id(zone), "{zone}");
        }
    }
}
