//! Install / refresh the `zd-exec` binary + `zd-runtime/` symlink
//! farm into app-private locations decoupled from the Termux-flavored
//! `$PREFIX`.
//!
//! `zd-exec` is the Rust spawn wrapper that routes every PATH-resolved
//! invocation (bash, git, rust-analyzer, …) through the active
//! `RuntimeProvider` (chroot / bootstrap / external Termux). The APK
//! ships it as an asset (`android/app/src/main/assets/zd-exec`) and
//! we extract it to `<data>/files/bin/zd-exec` at boot. The Gradle
//! `buildZdExec` task in `app/build.gradle.kts` produces the asset by
//! running `cargo ndk … build --release -p zdroid_runtime --bin
//! zd-exec` before each APK build, so the binary stays in lockstep
//! with the Rust libs inside the APK.
//!
//! Pre-Phase-4 these lived under `$PREFIX/bin/zd-exec` and
//! `$PREFIX/zd-runtime/`. Phase 4 of the Termux-divestment refactor
//! relocated them off `$PREFIX` so chroot mode doesn't depend on the
//! bootstrap-flavored layout. Existing installs get the orphans swept
//! on next boot; new installs land at the new paths directly.
//!
//! Why APK-bundled instead of part of the bootstrap zip: zd-exec is
//! tied to the editor's Rust code (RuntimeProvider trait, wire
//! protocol with zd-spawnd, etc.) — bumping the APK with a new
//! adapter or protocol revision must always bring a matching zd-exec.
//! The Termux bootstrap zip is independently versioned for the
//! userland (libc, coreutils, package set) and shouldn't drag in our
//! Rust binaries. Two artifacts, two cadences.
//!
//! Idempotent: a bounded-memory content comparison decides whether to
//! re-extract, including upgrades whose ELF size does not change.

use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use android_activity::AndroidApp;
use anyhow::{Context, Result, anyhow};

/// Asset file name inside the APK. Matches the staging path the
/// `buildZdExec` Gradle task writes to in `assets/`.
const ASSET_NAME: &str = "zd-exec";

/// Ensure `<data_path>/bin/zd-exec` exists and matches the APK-bundled
/// asset. Re-extracts if the file is missing or its contents differ
/// from the asset. No-op when the on-disk file already matches.
///
/// Sweeps the pre-Phase-4 `<data_path>/usr/bin/zd-exec` orphan if
/// present so existing installs upgrading through this change don't
/// leave ~5 MB of dead bytes behind.
pub fn ensure_installed(android_app: &AndroidApp, data_path: &Path) -> Result<()> {
    let stale = data_path.join("usr/bin").join(ASSET_NAME);
    if stale.exists() {
        match fs::remove_file(&stale) {
            Ok(()) => log::info!(
                "zd_exec_install: swept pre-Phase-4 stale binary at {}",
                stale.display()
            ),
            Err(err) => log::warn!(
                "zd_exec_install: could not remove stale {}: {err}",
                stale.display()
            ),
        }
    }

    let target = data_path.join("bin").join(ASSET_NAME);

    let asset_manager = android_app.asset_manager();
    let asset_name = CString::new(ASSET_NAME)?;
    let mut asset = asset_manager
        .open(&asset_name)
        .ok_or_else(|| anyhow!("{ASSET_NAME} asset not present in APK; check `buildZdExec` Gradle task ran"))?;
    let expected_len = asset.length();

    // Release builds can change while keeping exactly the same ELF byte
    // length, so a size-only check could leave an old spawn bridge in place
    // after an APK upgrade.
    if let Ok(meta) = fs::metadata(&target)
        && meta.len() as usize == expected_len
        && asset_matches_file(&mut asset, &target)?
    {
        log::debug!(
            "zd_exec_install: {} up to date ({} bytes); skipping",
            target.display(),
            expected_len,
        );
        return Ok(());
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
    // Write atomically: write to <target>.new, rename into place. The
    // intermediate name avoids leaving a half-written zd-exec if we
    // crash mid-write.
    let staging = target.with_extension("new");
    fs::write(&staging, &buf)
        .with_context(|| format!("write staging {}", staging.display()))?;
    let mut perms = fs::metadata(&staging)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&staging, perms)
        .with_context(|| format!("chmod 0755 {}", staging.display()))?;
    fs::rename(&staging, &target)
        .with_context(|| format!("rename {} -> {}", staging.display(), target.display()))?;

    log::info!(
        "zd_exec_install: extracted {} ({} bytes) -> {}",
        ASSET_NAME,
        expected_len,
        target.display(),
    );
    Ok(())
}

/// Populate `<data_path>/zd-runtime/` with one symlink per name in
/// `binaries`, each pointing at `../bin/zd-exec`. This is the
/// virtual `/usr/bin` the Zed app process gets to see — kernel PATH
/// lookup of e.g. `java` finds the symlink, exec's `zd-exec`, which
/// dispatches through the active `RuntimeProvider` to wherever the
/// binary actually lives.
///
/// `binaries` should come from `provider.list_binaries()` so the
/// dir mirrors the active adapter's filesystem. No hardcoded list:
/// when the user `apt install`s a new tool inside chroot, the next
/// boot picks it up; when they switch adapters, the symlink set is
/// rewritten to match the new env.
///
/// Idempotent and self-healing:
///
///   - Symlinks pointing at `../bin/zd-exec` whose name is NOT in
///     `binaries` are removed (so adapter switches and uninstalls
///     don't leave dead entries).
///   - Symlinks pointing at `../bin/zd-exec` whose name IS in
///     `binaries` are left alone.
///   - Non-symlink entries (file, dir) are left alone — we don't
///     touch what we didn't create.
///   - Names in `binaries` that don't yet exist are created.
///
/// Sweeps the pre-Phase-4 `<data_path>/usr/zd-runtime/` orphan dir
/// before populating the new location so existing installs upgrading
/// through this change shed the old symlink farm in one go.
pub fn ensure_runtime_symlinks(data_path: &Path, binaries: &[String]) -> Result<()> {
    let stale_dir = data_path.join("usr/zd-runtime");
    if stale_dir.exists() {
        match fs::remove_dir_all(&stale_dir) {
            Ok(()) => log::info!(
                "zd_exec_install: swept pre-Phase-4 stale dir at {}",
                stale_dir.display()
            ),
            Err(err) => log::warn!(
                "zd_exec_install: could not remove stale {}: {err}",
                stale_dir.display()
            ),
        }
    }

    let runtime_dir = data_path.join("zd-runtime");
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("create_dir_all {}", runtime_dir.display()))?;
    let target = Path::new("../bin/zd-exec");

    // Always include `zd-exec` itself in the symlink set so kernel PATH
    // lookup of the bare name `zd-exec` resolves to this dir first.
    // The env-root bridge in `util::command::Command::new` invokes
    // `Command::new("zd-exec").arg(<abs_path>)` for absolute paths
    // under env_root; without this symlink that lookup would either
    // miss entirely or fall through to a wrong binary on PATH.
    //
    // `zd-exec` doesn't live inside any adapter's rootfs (it's our
    // own binary at `$PREFIX/bin/zd-exec`), so adapter `list_binaries`
    // never returns it. We inject it here so the invariant lives next
    // to the symlink-creation logic instead of being duplicated by
    // every caller.
    let mut binaries: Vec<String> = binaries.to_vec();
    if !binaries.iter().any(|n| n == "zd-exec") {
        binaries.push("zd-exec".to_string());
    }
    let binaries = &binaries;

    let wanted: std::collections::HashSet<&str> =
        binaries.iter().map(String::as_str).collect();

    // Sweep stale: remove zd-exec-shaped symlinks whose name isn't
    // wanted anymore (typical after an adapter switch).
    let mut removed = 0usize;
    if let Ok(entries) = fs::read_dir(&runtime_dir) {
        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if wanted.contains(name.as_str()) {
                continue;
            }
            let path = entry.path();
            // Only remove if it's a symlink pointing where we'd point.
            // Anything else (real file, foreign symlink) is left alone.
            if let Ok(link_target) = fs::read_link(&path)
                && link_target == target
            {
                if let Err(e) = fs::remove_file(&path) {
                    log::warn!(
                        "zd_exec_install: failed to remove stale symlink {}: {e}",
                        path.display(),
                    );
                } else {
                    removed += 1;
                }
            }
        }
    }

    // Create or repair entries for everything in `binaries`.
    let mut created = 0usize;
    let mut already = 0usize;
    let mut skipped = 0usize;
    for name in binaries {
        let link = runtime_dir.join(name);
        match fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if fs::read_link(&link).ok().as_deref() == Some(target) {
                    already += 1;
                    continue;
                }
                // Symlink exists but points somewhere unexpected. Replace.
                fs::remove_file(&link).with_context(|| {
                    format!("removing wrong-target symlink {}", link.display())
                })?;
            }
            Ok(_) => {
                // Real file / directory at this name. Don't overwrite.
                skipped += 1;
                continue;
            }
            Err(_) => {} // Doesn't exist yet, create.
        }

        std::os::unix::fs::symlink(target, &link).with_context(|| {
            format!("symlink {} -> {}", link.display(), target.display())
        })?;
        created += 1;
    }

    log::info!(
        "zd_exec_install: zd-runtime ready at {} \
         (requested {}, created {}, already linked {}, removed stale {}, skipped non-symlinks {})",
        runtime_dir.display(),
        binaries.len(),
        created,
        already,
        removed,
        skipped,
    );
    Ok(())
}

/// Compares an APK asset stream with an installed file in fixed-size
/// chunks, so neither binary has to be held in memory twice.
pub(crate) fn asset_matches_file(asset: &mut impl Read, target: &Path) -> Result<bool> {
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
