//! Shared env-mutation primitive used by adapter env contracts on
//! Android. Lives in `util` so both `zdroid_runtime` (which produces
//! [`EnvOp`] lists per adapter) and `terminal` (which consumes them
//! when spawning the integrated terminal) can reference the same
//! type without a dependency cycle.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

/// One mutation in an adapter's env contract.
///
/// Adapters return a `Vec<(String, EnvOp)>` describing how the active
/// runtime wants the Zed-Rust process env (or a terminal-spawn env)
/// shaped. The caller iterates the list and applies each op via
/// `std::env::set_var` / `std::env::remove_var` (process-side) or by
/// mutating a `HashMap<String, String>` (per-spawn side).
#[derive(Debug, Clone)]
pub enum EnvOp {
    /// Set the variable to this value. Existing value, if any, is
    /// overwritten.
    Set(OsString),
    /// Remove the variable. No-op if it's not currently set.
    Remove,
}

/// Adapter-supplied overrides the integrated terminal applies on top of
/// the Zed-process env when spawning a PTY. The Zdroid Android port
/// registers this once at boot from
/// `RuntimeProvider::env_for_terminal`; `crates/terminal/src/
/// terminal.rs` reads it during pty env construction.
///
/// Lives here (in leaf `util`) rather than in `terminal` so the
/// adapter crate (`zdroid_runtime`) can register without taking a
/// dependency on the editor's terminal stack.
static TERMINAL_ENV_OVERLAY: OnceLock<Vec<(String, EnvOp)>> = OnceLock::new();

/// Install the active runtime adapter's terminal env overlay. Called
/// once at android_main; subsequent calls are silently dropped to
/// keep activity-recreation re-entry safe. Adapters return an empty
/// Vec when they have no overlay to contribute.
pub fn register_terminal_env_overlay(ops: Vec<(String, EnvOp)>) {
    if TERMINAL_ENV_OVERLAY.set(ops).is_err() {
        log::warn!("util::env: terminal env overlay already registered; ignoring re-register");
    }
}

/// Returns the registered overlay, or an empty slice if no adapter
/// registered one (non-Android builds, or pre-boot ordering bugs).
pub fn terminal_env_overlay() -> &'static [(String, EnvOp)] {
    TERMINAL_ENV_OVERLAY.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Active adapter's workspace root: where user projects + Zdroid-
/// managed cache live. Recent-projects UI groups paths under this
/// dir as "Workspace", others as "External"; `gpui_android::storage`
/// derives `~/projects` and the noexec-suppression cache from this.
///
/// Registered once at android_main from
/// `RuntimeProvider::workspace_root`. Returns `None` when no adapter
/// registered (non-Android builds or before runtime setup).
static WORKSPACE_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn register_workspace_root(path: Option<PathBuf>) {
    if WORKSPACE_ROOT.set(path).is_err() {
        log::warn!("util::env: workspace_root already registered; ignoring re-register");
    }
}

pub fn workspace_root() -> Option<&'static PathBuf> {
    WORKSPACE_ROOT.get().and_then(Option::as_ref)
}

/// Active adapter's `libtermux-exec.so` path, for editor code that
/// LD_PRELOADs it when spawning bionic CLIs that have hardcoded
/// `/data/data/com.termux/` paths baked in (Bun-compiled Termux npm
/// packages: claude, codex). Bootstrap is the only adapter that
/// returns Some; chroot returns None.
///
/// Registered once at android_main from
/// `RuntimeProvider::npm_libtermux_exec_path`.
static NPM_LIBTERMUX_EXEC_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn register_npm_libtermux_exec_path(path: Option<PathBuf>) {
    if NPM_LIBTERMUX_EXEC_PATH.set(path).is_err() {
        log::warn!("util::env: npm_libtermux_exec_path already registered; ignoring re-register");
    }
}

pub fn npm_libtermux_exec_path() -> Option<&'static PathBuf> {
    NPM_LIBTERMUX_EXEC_PATH.get().and_then(Option::as_ref)
}
