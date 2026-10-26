//! The runtime provider port — the contract every adapter satisfies.
//!
//! `zd-exec` and the settings UI both code against this trait. Adapters
//! plug in; nothing else cares which adapter is active.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use util::env::EnvOp;

use crate::config::RuntimeId;
use crate::health::{HealthStatus, ProgressSink};

/// A spawn request handed to the active provider. Mirrors the data a
/// `Command::new` carries plus the inheritable fds (passed via
/// `SCM_RIGHTS` in chroot mode).
///
/// Adapters take this and return a [`SpawnHandle`] representing the
/// running process. The caller waits / signals via the handle.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Target binary name as the user / Zed wants it (e.g. `git`,
    /// `rust-analyzer`). The adapter resolves this against its userland
    /// (e.g. `/usr/bin/git` inside the guest rootfs).
    pub program: String,

    /// argv[1..] for the child. argv[0] is set by the adapter to match
    /// `program` (or its absolute path, depending on lookup semantics).
    pub args: Vec<OsString>,

    /// Working directory the child should land in. Adapters translate
    /// host-side paths to their userland's view (e.g.
    /// `/data/.../home/projects/foo` → `/zed/projects/foo` inside a
    /// chroot whose `/zed` is bind-mounted to the host's home).
    pub cwd: Option<PathBuf>,

    /// Env vars to set for the child. The adapter merges with whatever
    /// baseline its userland needs (PATH inside the rootfs, HOME,
    /// TERM, etc.). Anything in this map wins.
    pub env: HashMap<String, OsString>,

    /// True if this spawn is replacing an interactive terminal. Adapters
    /// use this to decide whether to allocate a controlling tty,
    /// pass-through `setsid -c`, source a login shell, etc. Non-tty
    /// spawns (LSPs, git, formatters) skip those costs.
    pub interactive: bool,

    /// Stdio fds to pass to the child. Indexed 0=stdin, 1=stdout,
    /// 2=stderr. Typically `[0, 1, 2]` for "inherit the caller's
    /// stdio". Adapters that bridge across processes (chroot via its
    /// daemon) duplicate these fds across the boundary using
    /// `SCM_RIGHTS`. The fds remain valid for the caller; ownership is
    /// not transferred.
    pub stdio: [RawFd; 3],
}

/// A running child process spawned through an adapter. Exact mechanism
/// is adapter-specific (subprocess pid, daemon-tracked job, intent
/// callback handle), but the surface is uniform.
pub trait SpawnHandle: Send {
    /// Block until the child exits, return its exit code. -1 if killed
    /// by a signal that doesn't map to an exit code (the adapter logs
    /// the signal number).
    fn wait(&mut self) -> anyhow::Result<i32>;

    /// Send SIGKILL (or adapter-equivalent termination signal).
    fn kill(&mut self) -> anyhow::Result<()>;
}

/// The port. Implementations live in `adapters`.
pub trait RuntimeProvider: Send + Sync {
    /// Stable identifier for this provider — picked into `runtime.toml`
    /// and shown in the settings UI.
    fn id(&self) -> RuntimeId;

    /// Quick feasibility check ("ping"). Should complete in <1s for
    /// healthy state, <3s for failure modes. Settings UI calls this on
    /// page render to show a green/yellow/red dot per adapter.
    fn health_check(&self) -> HealthStatus;

    /// One-shot install / setup. Idempotent: calling on an
    /// already-installed adapter is a no-op + Ok. Long operations
    /// (downloading a rootfs tarball, extracting it) report
    /// progress through the sink.
    fn install(&self, progress: &mut dyn ProgressSink) -> anyhow::Result<()>;

    /// Remove anything this adapter installed. Chroot is a no-op (the
    /// rootfs is the user's, not ours); the rootfs adapter removes what
    /// it downloaded. Project files / settings / config are never
    /// touched.
    fn uninstall(&self) -> anyhow::Result<()>;

    /// Spawn the child process described by `req`. See [`SpawnHandle`]
    /// for the returned handle's contract.
    fn spawn(&self, req: SpawnRequest) -> anyhow::Result<Box<dyn SpawnHandle>>;

    /// Whether `util::command::new_command` should rewrite absolute-path
    /// spawns under [`Self::environment_root`] to route through
    /// `zd-exec`. Adapters that cross a namespace boundary (e.g. chroot)
    /// return true so the spawn lands inside the right userland;
    /// adapters whose binaries run natively on the host return false so
    /// `execve` runs the binary directly.
    ///
    /// Default false — opt in. A misregistered true value in an
    /// adapter without `zd-exec` on PATH fails every absolute-path
    /// spawn with ENOENT (no `zd-exec` resolvable from kernel PATH
    /// lookup); a missed true on a real chroot adapter dumps a glibc
    /// binary into bionic and gets a bad-interpreter error. The
    /// former is loud and immediate; pick that side of the
    /// asymmetry.
    fn needs_command_bridge(&self) -> bool {
        false
    }

    /// Host-side path that becomes Zed's entire data root when this
    /// adapter is active. The Zdroid Android port calls
    /// `paths::set_custom_data_dir(provider.environment_root())` at
    /// boot; every `paths::*` accessor (config, db, logs, languages,
    /// extensions, themes, …) then derives from here.
    ///
    /// In other words: switching the active adapter is a full Zed
    /// workspace switch. Settings, keymap, themes, sqlite database,
    /// extension installs, LSP downloads, project history — all of it
    /// is per-adapter. Nothing leaks across. This avoids the cognitive
    /// mess of "X lives shared, Y lives per-env"; the answer is always
    /// "whatever the active adapter says".
    ///
    /// Two invariants the returned path MUST satisfy:
    ///
    ///   1. **Reachable from the Zed app process.** Zed reads/writes
    ///      its entire state under this path (config, db, logs, …) so
    ///      it has to be a normal host filesystem path.
    ///
    ///   2. **Reachable from anything `spawn()` lands.** A chrooted
    ///      `node` exec'd by [`Self::spawn`] must be able to `require`
    ///      files under this dir. For the chroot adapter that means
    ///      the path lives inside the bind-mount source so the same
    ///      bytes are visible at a (different) absolute path inside
    ///      the chroot; the adapter's argv translation rewrites host
    ///      paths to chroot-target paths before forwarding to the
    ///      daemon.
    ///
    /// Adapter switching is a workspace switch: each adapter's
    /// `environment_root` is a separate physical location, populated
    /// independently. Files installed in one never leak into another.
    fn environment_root(&self) -> PathBuf;

    /// Names of binaries reachable in this adapter's PATH. Used by
    /// boot to populate `$PREFIX/zd-runtime/<name> -> ../bin/zd-exec`
    /// symlinks, which is what makes Zed's desktop-style PATH lookup
    /// (`Command::new("java")`) work transparently on Android — the
    /// kernel finds the symlink, exec's `zd-exec`, which dispatches
    /// through this provider into the right environment.
    ///
    /// On desktop OSes Zed never needs this: `/usr/bin/java` is on
    /// the user's PATH and lives on the same filesystem as the
    /// editor. On Android, the editor sits in a separate sandbox
    /// from the toolchain's filesystem namespace, so PATH lookup
    /// from the app process can't see the guest's `bin/` directly.
    /// The symlink farm at `zd-runtime/` is the
    /// virtual `/usr/bin` Zed gets to see; this method tells boot
    /// what to put in it.
    ///
    /// Default impl returns empty — adapters without a readable bin
    /// layout opt out cleanly; PATH-based lookup just won't find
    /// anything, which is the correct fail-closed behavior.
    fn list_binaries(&self) -> Vec<String> {
        Vec::new()
    }

    /// Mutations to apply to the Zed-Rust process env at android_main.
    ///
    /// Each active runtime adapter is the single source of truth for
    /// the Zed-process env shape — `HOME`, `PATH`, `SHELL`, `TMPDIR`,
    /// any Termux-flavored or chroot-flavored vars that downstream
    /// editor code reads. lib.rs no longer hardcodes anything; it
    /// asks the active provider and applies the result.
    ///
    /// `data_path` is the Zdroid app's private files root (= what
    /// `AndroidApp::internal_data_path()` returns), passed in so the
    /// adapter doesn't have to re-derive it from its config.
    ///
    /// Default impl is empty so non-Android adapters (none exist yet,
    /// but future ones) opt in deliberately.
    fn env_for_zed_process(&self, _data_path: &Path) -> Vec<(String, EnvOp)> {
        Vec::new()
    }

    /// Mutations to overlay on the integrated terminal's env at spawn
    /// time. Applied on top of whatever the Zed-Rust process env
    /// already has (i.e. on top of [`Self::env_for_zed_process`]).
    ///
    /// Used by `crates/terminal/src/terminal.rs` via the static
    /// `register_terminal_env_contributor` slot. Adapters that want
    /// the terminal to behave identically to the editor process can
    /// return an empty Vec — the inherited process env is then the
    /// terminal's env.
    fn env_for_terminal(&self, _data_path: &Path) -> Vec<(String, EnvOp)> {
        Vec::new()
    }

    /// Absolute path of the program the integrated terminal should
    /// exec when the user opens a new terminal. Chroot returns the
    /// `zd-exec` wrapper (which dispatches into the rootfs).
    ///
    /// Returns `None` for non-Android adapters to keep the upstream
    /// terminal code path untouched.
    fn terminal_shell(&self, _data_path: &Path) -> Option<PathBuf> {
        None
    }

    /// Where the user's projects and Zdroid-managed cache live.
    /// Recent-project UI groups paths under this dir as "Workspace"
    /// (vs. SAF-picked "External"); `gpui_android::storage` derives
    /// the `~/projects` mkdir target and the noexec-suppression cache
    /// from this. Returns `None` when the adapter has no opinion.
    fn workspace_root(&self, _data_path: &Path) -> Option<PathBuf> {
        None
    }
}
