//! External Termux adapter — bridges to the user's installed Termux app
//! via the `com.termux.RUN_COMMAND` intent service.
//!
//! Fire-and-forget short commands work cleanly through Termux's intent
//! API. Interactive shells / long-lived bidirectional-stdio LSPs need a
//! Termux-side helper script that opens a backing Unix socket — that
//! pattern lands in a follow-up commit. For v1 the adapter is
//! structurally complete (`health_check` probes the installed package,
//! `install` returns the on-screen setup guide steps) but `spawn`
//! deliberately errors with a clear "implementation pending" message
//! until the JNI Intent + stdio-bridge plumbing lands.
//!
//! Setup the user has to do once:
//! 1. Install Termux (F-Droid build, NOT the abandoned Play Store one).
//! 2. In Termux: `echo allow-external-apps=true >> ~/.termux/termux.properties`
//! 3. In Android settings: grant `com.zdroid` the
//!    `com.termux.permission.RUN_COMMAND` permission.
//! 4. Pick "Existing Termux app" in Zdroid's Runtime settings.
//!
//! Reference: https://github.com/termux/termux-app/wiki/RUN_COMMAND-Intent

use crate::config::{ExternalTermuxConfig, RuntimeId};
use crate::health::{HealthStatus, ProgressSink};
use crate::port::{RuntimeProvider, SpawnHandle, SpawnRequest};

/// Intent action Termux's `RunCommandService` listens for.
pub const RUN_COMMAND_ACTION: &str = "com.termux.RUN_COMMAND";

/// Service component name to target with `setClassName`.
pub const RUN_COMMAND_SERVICE_CLASS: &str = "com.termux.app.RunCommandService";

/// Extras for the Intent. All required (BACKGROUND defaults to false
/// if absent). Names must match the constants in Termux's source —
/// see `RunCommandService.java`.
pub mod extras {
    /// Absolute path to the executable. Typically
    /// `/data/data/com.termux/files/usr/bin/<name>`.
    pub const PATH: &str = "com.termux.RUN_COMMAND_PATH";
    /// `String[]` of argv after argv[0].
    pub const ARGUMENTS: &str = "com.termux.RUN_COMMAND_ARGUMENTS";
    /// Working directory. Termux `cd`s here before exec.
    pub const WORKDIR: &str = "com.termux.RUN_COMMAND_WORKDIR";
    /// Boolean: true means no UI session (no terminal opens in Termux),
    /// false means the user sees a terminal session for this command.
    pub const BACKGROUND: &str = "com.termux.RUN_COMMAND_BACKGROUND";
    /// Action to take with the session post-command:
    /// 0 = open in app, 1 = open and switch, 2 = no UI.
    pub const SESSION_ACTION: &str = "com.termux.RUN_COMMAND_SESSION_ACTION";
    /// `PendingIntent` to fire when the command completes. Result
    /// extras are written into this intent (stdout, stderr, exit code).
    pub const RESULT_PENDING_INTENT: &str = "com.termux.RUN_COMMAND_RESULT_PENDING_INTENT";
}

/// Bridges to the user's existing Termux app via Android intents.
/// Slowest of the three adapters (~80ms / spawn from intent
/// serialization + cross-process IPC) but uses the user's already
/// configured Termux: their installed packages, their proot-distro,
/// their `.bashrc`, all of it.
pub struct ExternalTermuxAdapter {
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    config: ExternalTermuxConfig,
}

impl ExternalTermuxAdapter {
    pub fn new(config: ExternalTermuxConfig) -> anyhow::Result<Self> {
        Ok(Self { config })
    }
}

impl RuntimeProvider for ExternalTermuxAdapter {
    fn id(&self) -> RuntimeId {
        RuntimeId::ExternalTermux
    }

    fn health_check(&self) -> HealthStatus {
        // TODO: PackageManager probe via JNI. Until plumbed, surface
        // a structured "needs setup" status that the settings UI
        // renders with the install button enabled.
        HealthStatus::NotInstalled {
            hint: format!(
                "External Termux adapter is structurally in place, but the JNI Intent bridge is not yet wired. \
                 Once wired, this check will probe `{}` via PackageManager and verify the RUN_COMMAND \
                 permission is granted.",
                self.config.package,
            ),
        }
    }

    fn install(&self, _progress: &mut dyn ProgressSink) -> anyhow::Result<()> {
        anyhow::bail!(
            "ExternalTermuxAdapter::install pending JNI Intent bridge. \
             Setup the user must do manually until then: \
             (1) install Termux from F-Droid, \
             (2) in Termux run `echo allow-external-apps=true >> ~/.termux/termux.properties`, \
             (3) grant `com.termux.permission.RUN_COMMAND` to `com.zdroid` in Android app permissions."
        )
    }

    fn uninstall(&self) -> anyhow::Result<()> {
        // Termux is the user's app; not ours to remove.
        Ok(())
    }

    fn spawn(&self, _req: SpawnRequest) -> anyhow::Result<Box<dyn SpawnHandle>> {
        // The wire is more involved than chroot's: an Intent serializes
        // command + args + workdir + a result PendingIntent, but stdio
        // can't ride along (Intents don't carry fds). Two follow-ups:
        //   1. JNI helpers to construct the Intent and call startService.
        //   2. A Termux-side `zd-bridge` helper that the RUN_COMMAND
        //      target invokes with paths to two abstract Unix sockets
        //      (one for stdin, one for stdout/stderr); the helper opens
        //      them, runs the actual target with those fds dup'd in,
        //      and we read/write across the boundary.
        // Until both land, refuse cleanly so callers know to fall back.
        anyhow::bail!(
            "ExternalTermuxAdapter::spawn pending: Intent IPC bridge + Termux-side stdio helper not yet implemented"
        )
    }

    fn environment_root(&self) -> std::path::PathBuf {
        // Termux's `$PREFIX/.zed-env/`. Zdroid (com.zdroid, separate uid)
        // can't write here directly — Termux's data dir is private to
        // com.termux. The eventual JNI Intent bridge will route filesystem
        // ops here through Termux's RUN_COMMAND service. Returning the
        // path now so the seam is in place; actual writes will fail
        // until the bridge lands.
        self.config.prefix.join(".zed-env")
    }

    fn env_for_zed_process(&self, data_path: &std::path::Path) -> Vec<(String, util::env::EnvOp)> {
        // External Termux mode: spawns are dispatched via Intent to
        // com.termux, so the Zed-Rust process doesn't propagate any
        // Termux-flavored env into subprocesses. Keep the Zed-process
        // env bionic-clean and minimal. The JNI Intent bridge (task
        // #36) will carry per-spawn env across the package boundary
        // when it lands; nothing here leaks across.
        use std::ffi::OsString;
        use util::env::EnvOp;

        let existing_path = std::env::var_os("PATH").unwrap_or_default();

        vec![
            ("HOME".into(), EnvOp::Set(data_path.as_os_str().to_owned())),
            (
                "TMPDIR".into(),
                EnvOp::Set(data_path.join("tmp").into_os_string()),
            ),
            ("TERM".into(), EnvOp::Set(OsString::from("xterm-256color"))),
            ("COLORTERM".into(), EnvOp::Set(OsString::from("truecolor"))),
            ("LANG".into(), EnvOp::Set(OsString::from("en_US.UTF-8"))),
            (
                "ZED_BUILD_REMOTE_SERVER".into(),
                EnvOp::Set(OsString::from("never")),
            ),
            ("LD_PRELOAD".into(), EnvOp::Remove),
            ("PATH".into(), EnvOp::Set(existing_path)),
        ]
    }

    fn env_for_terminal(&self, _data_path: &std::path::Path) -> Vec<(String, util::env::EnvOp)> {
        // No overlay — the integrated terminal stub for external
        // Termux is a placeholder until the Intent bridge lands. The
        // PTY just hosts whatever shell `terminal_shell` returns; once
        // the bridge ships, terminal spawns will route via Intent and
        // env propagation happens at that boundary.
        Vec::new()
    }

    fn terminal_shell(&self, _data_path: &std::path::Path) -> Option<std::path::PathBuf> {
        // Bionic system shell until the Intent bridge lets us launch
        // a Termux-side bash. /system/bin/sh exists on every Android
        // build by definition.
        Some(std::path::PathBuf::from("/system/bin/sh"))
    }
}
