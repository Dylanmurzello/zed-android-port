//! Health of the runtime and progress reporting for its long operations.

/// Outcome of [`crate::Runtime::health_check`].
#[derive(Debug, Clone)]
pub enum HealthStatus {
    /// The rootfs is installed and a guest program ran.
    Healthy,
    /// No rootfs under the data dir: first launch, or a failed setup.
    NotInstalled,
    /// The loader or the rootfs is there but a guest program did not run.
    Failed {
        /// First line for the UI; the full chain goes to the log.
        error: String,
    },
}

impl HealthStatus {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// Sink the install flows write progress to. The UI hands a sink that
/// updates a notification; headless paths pass [`LogProgressSink`].
///
/// Long operations (extraction, apt) call `progress` regularly so the
/// user does not think the app has hung, and poll `is_cancelled`
/// between chunks.
pub trait ProgressSink: Send {
    /// The high-level step the user sees ("Extracting", "Installing
    /// essentials").
    fn step(&mut self, label: &str);

    /// Progress within the current step in arbitrary units (entries for
    /// extraction). `total = 0` means unknown.
    fn progress(&mut self, done: u64, total: u64);

    /// A non-fatal warning the user should see.
    fn warn(&mut self, message: &str);

    /// Polled by long operations between chunks; `true` makes the
    /// operation stop and clean up its staging state.
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// `ProgressSink` for tests and headless invocations.
pub struct NoopProgressSink;

impl ProgressSink for NoopProgressSink {
    fn step(&mut self, _: &str) {}
    fn progress(&mut self, _: u64, _: u64) {}
    fn warn(&mut self, _: &str) {}
}

/// `ProgressSink` that forwards everything to the `log` crate.
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
