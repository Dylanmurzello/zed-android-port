//! In-app diagnostic exporter.
//!
//! Reporters without a PC + ADB can't dump our log output: apps need
//! the signature-level `READ_LOGS` permission to read another app's
//! logcat, and there's no user-facing grant path. So Zdroid keeps a
//! ring buffer of log lines worth shipping (see Kotlin `Diagnostic`
//! object) and exposes a menu action that hands the dump to Android's
//! share sheet. This module is the JNI bridge that menu handler calls.
//!
//! Pattern mirrors `updater.rs`: stash the `AndroidApp` once at boot
//! via [`register_android_app`], then [`export`] dispatches through
//! the activity's `exportDiagnostic(String)` instance method.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use android_activity::AndroidApp;
use anyhow::{Context as _, Result, anyhow};
use jni::{JavaVM, objects::JObject};

static ANDROID_APP: OnceLock<AndroidApp> = OnceLock::new();

pub fn register_android_app(app: AndroidApp) {
    let _ = ANDROID_APP.set(app);
}

/// Rust-side counterpart to Kotlin's `Diagnostic.ring`. Anything
/// recorded here is drained into the dump's `runtime state` section
/// at export time. Keeping it separate from the Kotlin ring avoids a
/// JNI hop on every captured-pointer arrival (motion events can hit
/// ~200Hz during a sustained trackpad drag); the cross-boundary cost
/// is paid once at export, not per-event.
///
/// Sized to mirror Kotlin's capacity so a single repro window fits
/// on each side.
const RUST_RING_CAPACITY: usize = 2000;
static RUST_RING: Mutex<Option<VecDeque<String>>> = Mutex::new(None);

pub(crate) fn record(line: String) {
    let Ok(mut guard) = RUST_RING.lock() else { return };
    let ring = guard.get_or_insert_with(VecDeque::new);
    if ring.len() >= RUST_RING_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(line);
}

fn snapshot_rust_ring() -> Vec<String> {
    let Ok(guard) = RUST_RING.lock() else {
        return Vec::new();
    };
    guard
        .as_ref()
        .map(|ring| ring.iter().cloned().collect())
        .unwrap_or_default()
}

/// Compose Rust-side state extras and hand off to Kotlin to build the
/// dump file and fire the share intent. Idempotent in the failure
/// case — any error is logged and swallowed so a broken diagnostic
/// export never takes down the app.
pub fn export() {
    let extras = compose_runtime_extras();
    if let Err(err) = call_export(&extras) {
        log::warn!("diagnostic: export failed: {err:#}");
    }
}

fn compose_runtime_extras() -> String {
    let mut s = String::new();
    s.push_str("env:\n");
    // Keys chosen to triage runtime-selection / spawn / termux paths.
    // Bionic process env, not envp; matches what subprocess spawns see.
    const KEYS: &[&str] = &[
        "PATH",
        "SHELL",
        "HOME",
        "PREFIX",
        "TMPDIR",
        "ZED_RELEASE_CHANNEL",
        "ZD_RUNTIME_CONFIG",
        "ZDROID_ADAPTER",
        "ANDROID_DATA",
        "ANDROID_ROOT",
        "LD_PRELOAD",
    ];
    for k in KEYS {
        match std::env::var(k) {
            Ok(v) => s.push_str(&format!("  {k}={v}\n")),
            Err(_) => s.push_str(&format!("  {k}=(unset)\n")),
        }
    }
    s.push_str(&format!(
        "gpui_android_pkg_version: {}\n",
        env!("CARGO_PKG_VERSION")
    ));
    let lines = snapshot_rust_ring();
    if !lines.is_empty() {
        s.push_str(&format!("\nrust_ring ({} lines):\n", lines.len()));
        for line in lines {
            s.push_str("  ");
            s.push_str(&line);
            s.push('\n');
        }
    }
    s
}

fn call_export(extras: &str) -> Result<()> {
    let app = ANDROID_APP
        .get()
        .ok_or_else(|| anyhow!("AndroidApp not registered"))?;
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast())? };
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as _) };
    let extras_str = env.new_string(extras).context("new_string extras")?;
    env.call_method(
        &activity,
        "exportDiagnostic",
        "(Ljava/lang/String;)V",
        &[(&extras_str).into()],
    )?;
    Ok(())
}
