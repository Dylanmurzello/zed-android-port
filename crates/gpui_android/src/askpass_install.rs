//! Install the `zed-askpass-helper` binary from APK assets to an
//! app-private path.
//!
//! The askpass crate (`crates/askpass/src/askpass.rs`) sets `SSH_ASKPASS`
//! to a generated script that execs whatever path `askpass::set_askpass_program`
//! registered. Desktop platforms use `current_exe()` (the zed binary
//! itself, with `--askpass=<sock>`). On Android `current_exe()` is
//! `/system/bin/app_process64` (the Zygote launcher hosting the DEX
//! runtime), and ssh exec'ing it outside of an Activity context aborts
//! with `Error changing dalvik-cache ownership: Permission denied`. The
//! helper here is a tiny static aarch64 ELF (no JVM, no DEX), which the
//! runtime's ssh runs through the engine loader, doing the same netcat-style relay
//! the desktop `askpass::main` does: read prompt from stdin, write to a
//! Unix socket the gpui process is listening on, read the password
//! back, print it to stdout.
//!
//! It installs to `<data>/files/zed-askpass-helper`, which the runtime
//! sees at the same path.
//!
//! Idempotent: the installed copy is compared with the asset before
//! extracting again.

use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use android_activity::AndroidApp;
use anyhow::{Context, Result, anyhow};

/// Asset name inside the APK. Matches the staging path the Cargo build
/// emits in `crates/gpui_android/examples/zed_android/build.rs`.
const ASSET_NAME: &str = "zed-askpass-helper";

/// Ensure the askpass helper is present at `<data_path>/<ASSET_NAME>`
/// and matches the APK-bundled asset. Returns the absolute path to the
/// installed binary so the caller can hand it to `askpass::set_askpass_program`.
pub fn ensure_installed(android_app: &AndroidApp, data_path: &Path) -> Result<PathBuf> {
    let target = data_path.join(ASSET_NAME);

    let asset_manager = android_app.asset_manager();
    let asset_name = CString::new(ASSET_NAME)?;
    let mut asset = asset_manager.open(&asset_name).ok_or_else(|| {
        anyhow!("{ASSET_NAME} asset not present in APK; check the cargo build step bundled it")
    })?;
    let expected_len = asset.length();

    if let Ok(meta) = fs::metadata(&target)
        && meta.len() as usize == expected_len
        && asset_matches_file(&mut asset, &target)?
    {
        log::debug!(
            "askpass_install: {} up to date ({} bytes); skipping",
            target.display(),
            expected_len,
        );
        return Ok(target);
    }

    // The comparison consumes the asset stream; reopen it before extracting.
    let mut asset = asset_manager.open(&asset_name).ok_or_else(|| {
        anyhow!("{ASSET_NAME} asset disappeared while checking the installed copy")
    })?;

    let mut buf = Vec::with_capacity(expected_len);
    asset.read_to_end(&mut buf)?;

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let staging = target.with_extension("new");
    fs::write(&staging, &buf).with_context(|| format!("write staging {}", staging.display()))?;
    let mut perms = fs::metadata(&staging)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&staging, perms)
        .with_context(|| format!("chmod 0755 {}", staging.display()))?;
    fs::rename(&staging, &target)
        .with_context(|| format!("rename {} -> {}", staging.display(), target.display()))?;

    log::info!(
        "askpass_install: installed askpass helper ({} bytes) at {}",
        buf.len(),
        target.display()
    );
    Ok(target)
}

/// Compares an APK asset stream with an installed file in fixed-size
/// chunks, so neither binary has to be held in memory twice.
fn asset_matches_file(asset: &mut impl Read, target: &Path) -> Result<bool> {
    let mut installed = fs::File::open(target)?;
    let mut asset_buffer = [0_u8; 64 * 1024];
    let mut installed_buffer = [0_u8; 64 * 1024];
    loop {
        let read = asset.read(&mut asset_buffer)?;
        if read == 0 {
            return Ok(true);
        }
        if installed.read_exact(&mut installed_buffer[..read]).is_err()
            || asset_buffer[..read] != installed_buffer[..read]
        {
            return Ok(false);
        }
    }
}
