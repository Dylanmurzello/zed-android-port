//! Adapter health check + install-progress reporting.
//!
//! Shape is shared because settings UI calls them in the same flow:
//! "is this adapter healthy?" before running, "did install succeed?"
//! after. Both surface to the user as the same green/yellow/red dot
//! plus an optional reason string.

/// Outcome of [`crate::port::RuntimeProvider::health_check`]. Settings UI
/// renders these as a colored dot + tooltip.
#[derive(Debug, Clone)]
pub enum HealthStatus {
    /// Adapter is installed, configured, reachable. Ready to spawn.
    Healthy,

    /// The adapter's prerequisites aren't installed yet (e.g. chroot
    /// rootfs not at the configured path, or external Termux's
    /// `RunCommandService` permission not granted). UI offers to run
    /// `install()` or shows a setup hint.
    NotInstalled {
        /// One-line user-facing message. Short, actionable.
        hint: String,
    },

    /// Adapter is installed but broken — config disagrees with disk
    /// state (e.g. chroot rootfs is at the configured path but
    /// `zd-spawnd` socket isn't responding; the rootfs is at the
    /// configured path but its version stamp is older than required).
    Misconfigured {
        /// One-line description of what's wrong, plus a hint how to
        /// fix it ("Magisk module not running — try Reboot, or
        /// reinstall the module").
        reason: String,
    },

    /// Adapter raised an error during the check. Distinct from
    /// `Misconfigured` because the cause may be transient (e.g. a JNI
    /// thread-attach race).
    Failed {
        /// `anyhow::Error` rendered as a string. UI shows the first
        /// line as a one-liner; full chain is available in app logs.
        error: String,
    },
}

impl HealthStatus {
    /// True if `health_check` returned [`HealthStatus::Healthy`].
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// Sink the install/uninstall flows write progress messages to.
/// Settings UI hands a sink that updates a progress bar + status text;
/// command-line / smoke-test paths can pass a no-op or a stdout-printer.
///
/// Calls are advisory — adapters that genuinely have nothing to report
/// just don't call this at all. Long ops (rootfs download and
/// extraction) MUST call regularly so the user doesn't think the app
/// has hung.
pub trait ProgressSink: Send {
    /// Set the high-level step the user sees ("Downloading rootfs…",
    /// "Extracting…", "Verifying checksum…").
    fn step(&mut self, label: &str);

    /// Update the progress fraction within the current step. `done` and
    /// `total` are in arbitrary units (bytes for downloads, files for
    /// extraction). Adapters that can't report a meaningful fraction
    /// pass `total = 0` and the UI shows an indeterminate spinner.
    fn progress(&mut self, done: u64, total: u64);

    /// Note a non-fatal warning the user should see (e.g. "checksum
    /// mismatch on optional asset, falling back to source build").
    /// Surfaces in the install dialog's expandable "Details" section.
    fn warn(&mut self, message: &str);
}

/// No-op `ProgressSink` for unit tests and headless invocations.
pub struct NoopProgressSink;

impl ProgressSink for NoopProgressSink {
    fn step(&mut self, _: &str) {}
    fn progress(&mut self, _: u64, _: u64) {}
    fn warn(&mut self, _: &str) {}
}

/// `ProgressSink` that forwards everything to the `log` crate. For
/// headless install paths (zd-exec, smoke tests) where no UI is
/// listening.
pub struct LogProgressSink;

impl ProgressSink for LogProgressSink {
    fn step(&mut self, label: &str) {
        log::info!("zdroid_runtime: install: {label}");
    }
    fn progress(&mut self, done: u64, total: u64) {
        if total > 0 {
            log::debug!("zdroid_runtime: install: {done}/{total}");
        }
    }
    fn warn(&mut self, message: &str) {
        log::warn!("zdroid_runtime: install: {message}");
    }
}
