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
use jni::{JavaVM, objects::JObject};

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
    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread for storage permissions")?;
    let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };
    let result = env
        .call_method(&activity, "requestStoragePermissions", "()I", &[])
        .context("MainActivity.requestStoragePermissions")?
        .i()?;
    Ok(result)
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

/// Canonical local-projects root: `<termux_home>/projects`.
///
/// We read `TERMUX__HOME` (set by lib.rs at boot to `data_path/home`) rather
/// than `$HOME`, because lib.rs sets `$HOME` to `data_path` itself for
/// compatibility with code that expects HOME to point at the app's data
/// root, while the integrated terminal lands inside `data_path/home`.
/// Asks the active runtime adapter (via `util::env::workspace_root`)
/// for the right path; Bootstrap returns `<data>/files/home`, chroot
/// returns the same (visible via symmetric bind-mount). Returns None
/// only if no adapter registered yet (lib.rs hasn't run, external
/// Termux mode).
pub fn projects_dir() -> Option<PathBuf> {
    util::env::workspace_root().map(|root| root.join("projects"))
}

/// Path of the JSON file storing per-folder "suppress noexec warning"
/// entries. Lives under `<workspace_root>/.cache/zed-android/`. Returns
/// `None` only if no adapter registered a workspace_root yet.
fn noexec_suppressed_file() -> Option<PathBuf> {
    let root = util::env::workspace_root()?;
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

/// Outcome of a `copy_tree` invocation. The caller decides what to do with
/// the dst path on each variant — copy_tree never deletes (callers own that
/// because they generated the dst via the `<basename>-N` collision loop and
/// know it's safe to remove on cancel).
#[derive(Debug)]
pub enum CopyOutcome {
    /// Walk finished. `bytes` is the total successfully-copied byte count.
    Completed { bytes: u64, files: u64 },
    /// `cancel` flag flipped mid-walk. Caller is expected to `remove_dir_all`
    /// the dst path it allocated.
    Cancelled,
}

/// Snapshot pushed to the progress callback. Cheap to clone.
#[derive(Debug, Clone, Copy, Default)]
pub struct CopyProgress {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
}

/// Best-effort recursive copy from `src` to `dst`. Used by the noexec banner
/// Move action — copies a project from /storage/emulated/0 (FUSE noexec) to
/// ~/projects/<name> (app-private, exec-allowed) so cargo / go / make /
/// native build chains can actually run the resulting binaries.
///
/// Symlinks in the source are recreated as symlinks (not followed). Files
/// preserve mode bits. Errors on individual entries are logged and skipped —
/// a half-imported tree is more useful than a hard failure midway through.
///
/// `cancel`: checked between every entry. When set, the walk drops out and
/// returns `CopyOutcome::Cancelled` without rolling back. The caller is
/// expected to allocate `dst` via the existing `<basename>-N` collision
/// resolver and then `remove_dir_all(dst)` on cancel — that path is
/// guaranteed not to have contained user data before this call started.
///
/// `on_progress`: invoked after each entry with cumulative counters. Cheap
/// to call — the toast renderer can throttle on its own side if it wants.
pub fn copy_tree(
    src: &Path,
    dst: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    on_progress: &dyn Fn(CopyProgress),
) -> Result<CopyOutcome> {
    use std::sync::atomic::Ordering;

    if !src.is_dir() {
        anyhow::bail!("copy_tree: source {} is not a directory", src.display());
    }
    std::fs::create_dir_all(dst).with_context(|| format!("create_dir_all {}", dst.display()))?;
    let mut progress = CopyProgress::default();
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((cur_src, cur_dst)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(CopyOutcome::Cancelled);
        }
        let entries = match std::fs::read_dir(&cur_src) {
            Ok(e) => e,
            Err(err) => {
                log::warn!(
                    "storage: read_dir {}: {err:#}, skipping subtree",
                    cur_src.display()
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(CopyOutcome::Cancelled);
            }
            let entry_src = entry.path();
            let entry_dst = cur_dst.join(entry.file_name());
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(err) => {
                    log::warn!(
                        "storage: file_type {}: {err:#}, skipping",
                        entry_src.display()
                    );
                    continue;
                }
            };
            if file_type.is_symlink() {
                match std::fs::read_link(&entry_src) {
                    Ok(target) => {
                        let _ = std::fs::remove_file(&entry_dst);
                        if let Err(err) = std::os::unix::fs::symlink(&target, &entry_dst) {
                            log::warn!(
                                "storage: symlink {} -> {}: {err:#}",
                                entry_dst.display(),
                                target.display()
                            );
                        }
                    }
                    Err(err) => log::warn!("storage: read_link {}: {err:#}", entry_src.display()),
                }
            } else if file_type.is_dir() {
                if let Err(err) = std::fs::create_dir_all(&entry_dst) {
                    log::warn!("storage: mkdir {}: {err:#}", entry_dst.display());
                    continue;
                }
                progress.dirs += 1;
                stack.push((entry_src, entry_dst));
            } else if file_type.is_file() {
                match std::fs::copy(&entry_src, &entry_dst) {
                    Ok(n) => {
                        progress.bytes += n;
                        progress.files += 1;
                    }
                    Err(err) => log::warn!(
                        "storage: copy {} -> {}: {err:#}",
                        entry_src.display(),
                        entry_dst.display()
                    ),
                }
            }
            on_progress(progress);
        }
    }
    Ok(CopyOutcome::Completed {
        bytes: progress.bytes,
        files: progress.files,
    })
}

fn ensure_symlink(link: &Path, target: &Path) {
    match std::fs::read_link(link) {
        Ok(existing) if existing == target => return,
        Ok(_) => {
            // Symlink points somewhere else — refresh it.
            let _ = std::fs::remove_file(link);
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
