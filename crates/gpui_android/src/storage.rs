//! Runtime READ/WRITE_EXTERNAL_STORAGE prompt for Android.
//!
//! At `targetSdk = 28` we land in legacy-storage mode, but
//! `READ_EXTERNAL_STORAGE` and `WRITE_EXTERNAL_STORAGE` are still dangerous
//! permissions that Android won't grant at install time — they need a
//! runtime dialog. SAF folder picking dodges this for tree access, but the
//! moment `RealFs` reads `/storage/emulated/0/projects/foo.rs` directly
//! (which is what happens after the SAF picker hands us back a
//! `/storage/...` path), the syscall fails with `EACCES` until the user has
//! granted those perms.
//!
//! This module is the JNI bridge to MainActivity's `requestStoragePermissions()`,
//! which fires the dialog on first launch. Fire-and-forget by design — if
//! the user denies, file ops EACCES with a clean error and they can grant
//! later via Settings → Apps → zed_android → Permissions.
//!
//! The MANAGE_EXTERNAL_STORAGE escape hatch we used at `targetSdk=35` is
//! deliberately not part of this flow. That permission is API 30+ only and
//! requires a Settings deep-link rather than a runtime dialog; at
//! `targetSdk=28` it has no effect anyway.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use android_activity::AndroidApp;
use anyhow::{Context, Result};

static REQUESTED: OnceLock<()> = OnceLock::new();

/// Fire the `requestStoragePermissions()` JNI call once per process. Logs
/// the result code (1 = already granted, 0 = dialog posted). Re-entry from
/// activity recreation is a no-op via `OnceLock`.
pub fn request_once(android_app: &AndroidApp) {
    if REQUESTED.get().is_some() {
        return;
    }
    match request_inner(android_app) {
        Ok(code) => {
            log::info!(
                "storage: requestStoragePermissions returned {} ({})",
                code,
                if code == 1 {
                    "already granted"
                } else {
                    "dialog posted"
                }
            );
            let _ = REQUESTED.set(());
        }
        Err(err) => {
            // Don't latch the OnceLock on failure — let the next android_main
            // re-entry try again. Failures here are usually JNI thread-attach
            // races during early boot and recover after lifecycle settles.
            log::warn!("storage: requestStoragePermissions failed: {err:#}");
        }
    }
}

fn request_inner(android_app: &AndroidApp) -> Result<i32> {
    crate::jni_env::with_activity(android_app, |env, activity| {
        let result = env
            .call_method(activity, "requestStoragePermissions", "()I", &[])
            .context("MainActivity.requestStoragePermissions")?
            .i()?;
        Ok(result)
    })
}

/// Termux-style `~/storage/` curated symlinks into shared storage.
///
/// Mirrors what `termux-setup-storage` does in stock Termux: makes the common
/// /storage/emulated/0 subdirs available under $HOME/storage/<name> so users
/// can `cd ~/storage/downloads`, edit a file, save it back. /storage/emulated/0
/// is FUSE-mounted noexec, so this is browse / read / write only — never the
/// place to compile or run from. Workspaces live in $HOME/projects (created
/// alongside the symlinks) where exec is allowed.
///
/// Idempotent: rechecks every symlink target on each call. Ignores entries that
/// already point at the right place. Re-creates any that don't (covers the
/// case where /storage layout changed between launches, e.g. SD card swap).
pub fn setup_user_symlinks(termux_home: &Path) {
    let storage_dir = termux_home.join("storage");
    if let Err(err) = std::fs::create_dir_all(&storage_dir) {
        log::warn!("storage: create {}: {err:#}", storage_dir.display());
        return;
    }

    let primary = Path::new("/storage/emulated/0");
    let curated: &[(&str, PathBuf)] = &[
        ("shared", primary.to_path_buf()),
        ("dcim", primary.join("DCIM")),
        ("downloads", primary.join("Download")),
        ("documents", primary.join("Documents")),
        ("movies", primary.join("Movies")),
        ("music", primary.join("Music")),
        ("pictures", primary.join("Pictures")),
        ("podcasts", primary.join("Podcasts")),
    ];
    for (name, target) in curated {
        ensure_symlink(&storage_dir.join(name), target);
    }

    if let Ok(entries) = std::fs::read_dir("/storage") {
        let mut external_idx = 1;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Skip the user-emulated mount and the per-process self link —
            // they're not a removable volume. Real SD cards / OTG drives show
            // up as /storage/<UUID> (e.g. /storage/1DB9-0830).
            if name_str == "emulated" || name_str == "self" {
                continue;
            }
            let target = entry.path();
            ensure_symlink(
                &storage_dir.join(format!("external-{external_idx}")),
                &target,
            );
            external_idx += 1;
        }
    }
}

/// Local projects live under the active runtime's workspace root.
pub fn projects_dir() -> Option<PathBuf> {
    zdroid_hooks::workspace_root().map(|root| root.join("projects"))
}

/// Path of the JSON file storing per-folder "suppress noexec warning"
/// entries. Lives under `<workspace_root>/.cache/zed-android/`. Returns
/// `None` only if no adapter registered a workspace_root yet.
fn noexec_suppressed_file() -> Option<PathBuf> {
    let root = zdroid_hooks::workspace_root()?;
    let dir = root.join(".cache").join("zed-android");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("noexec-suppressed.json"))
}

fn read_noexec_suppressed_list() -> Vec<String> {
    let Some(file) = noexec_suppressed_file() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(&file) else {
        return Vec::new();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Returns true if the user has previously dismissed the noexec warning for
/// this exact path. The title-bar banner consults this before rendering.
pub fn is_noexec_suppressed(path: &Path) -> bool {
    let needle = path.to_string_lossy();
    read_noexec_suppressed_list()
        .iter()
        .any(|p| p.as_str() == needle)
}

/// Persist a "don't warn me about this path again" entry. No-op if the
/// path is already in the list. Best-effort — write errors are logged but
/// don't propagate (failure mode: user sees the banner again next session).
pub fn add_noexec_suppressed(path: &Path) {
    let Some(file) = noexec_suppressed_file() else {
        log::warn!(
            "noexec_suppressed: TERMUX__HOME unset; cannot persist suppression for {}",
            path.display()
        );
        return;
    };
    let mut list = read_noexec_suppressed_list();
    let needle = path.to_string_lossy().into_owned();
    if list.iter().any(|p| p == &needle) {
        return;
    }
    list.push(needle);
    let json = match serde_json::to_string_pretty(&list) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("noexec_suppressed: serialize failed: {err:#}");
            return;
        }
    };
    if let Err(err) = std::fs::write(&file, json) {
        log::warn!(
            "noexec_suppressed: write {} failed: {err:#}",
            file.display()
        );
    }
}

/// True if the filesystem `path` lives on is mounted with `noexec`. Returns
/// false on any error (couldn't statvfs, path missing, etc.) — false positives
/// are harmful (we'd nag users about a path that's actually fine), false
/// negatives are recoverable (user hits the EACCES we tried to predict).
///
/// Used by the title-bar banner: when a worktree's root sits on a noexec
/// mount (almost always /storage/emulated/0/* under Android's FUSE wrapper),
/// builds will EACCES at execve time. We surface this preemptively with a
/// "Move to ~/projects/" action.
pub fn is_noexec_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let path_c = match std::ffi::CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path_c.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    (buf.f_flag & libc::ST_NOEXEC) != 0
}

fn ensure_symlink(link: &Path, target: &Path) {
    match std::fs::read_link(link) {
        Ok(existing) if existing == target => return,
        Ok(_) => {
            // Symlink points somewhere else — refresh it.
            if let Err(error) = std::fs::remove_file(link) {
                log::warn!("storage: remove stale link {}: {error:#}", link.display());
                return;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            // Path exists but isn't a symlink (regular file/dir). Don't
            // clobber — log and move on. User can `rm` it themselves if
            // they want us to manage it.
            log::warn!(
                "storage: {} exists and isn't a symlink — leaving alone",
                link.display()
            );
            return;
        }
    }
    if let Err(err) = std::os::unix::fs::symlink(target, link) {
        log::warn!(
            "storage: symlink {} -> {} failed: {err:#}",
            link.display(),
            target.display()
        );
    }
}
