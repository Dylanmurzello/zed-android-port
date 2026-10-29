//! Where the engine loader is and how the editor's PATH reaches the
//! rootfs: the stable loader link, the `zd-runtime/` symlink farm, and
//! the bundled Ubuntu image the first launch extracts.
//!
//! The loader ships in the APK's native library dir, the one app-owned
//! location the kernel may exec from at any targetSdk. That directory
//! moves on every install, so everything persistent (the farm,
//! `terminal.shell`, `$SHELL`) names `<data>/files/bin/zdload` and boot
//! re-points that link. The farm is the virtual `/usr/bin` the editor
//! sees: kernel PATH lookup of `rust-analyzer` finds
//! `zd-runtime/rust-analyzer -> ../bin/zdload`, and the loader runs the
//! guest program of that name inside the rootfs.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use android_activity::AndroidApp;
use anyhow::{Context, Result, anyhow};
use jni::objects::JString;

/// The bundled Ubuntu base image, as a stream out of the APK. Gradle's
/// `fetchRootfsImage` task verified its checksum at build time and the
/// APK signature covers it since.
pub fn bundled_rootfs_image(android_app: &AndroidApp) -> Result<ndk::asset::Asset> {
    let asset_path = zdroid_runtime::install::IMAGE_ASSET_PATH;
    let asset_name = CString::new(asset_path)?;
    android_app
        .asset_manager()
        .open(&asset_name)
        .ok_or_else(|| {
            anyhow!("{asset_path} is not in the APK; the stageRootfsAsset gradle task did not run")
        })
}

/// Populate `<data_path>/zd-runtime/` with one symlink per name in
/// `binaries`, each pointing at `target` (`../bin/zdload`).
///
/// Idempotent and self-healing:
///
///   - Symlinks of ours whose name is NOT in `binaries` are removed, so
///     a tool removed from the rootfs leaves no dead entry.
///   - Symlinks of ours whose name IS in `binaries` are re-pointed when
///     the target changed, else left alone.
///   - Non-symlink entries (file, dir) are left alone; we don't touch
///     what we didn't create.
///   - Names in `binaries` that don't yet exist are created.
///
/// Entries written by earlier releases pointed at `../bin/zd-exec`; they
/// are ours to sweep too.
pub fn ensure_runtime_symlinks(data_path: &Path, binaries: &[String], target: &Path) -> Result<()> {
    let runtime_dir = data_path.join("zd-runtime");
    fs::create_dir_all(&runtime_dir)
        .with_context(|| format!("create_dir_all {}", runtime_dir.display()))?;
    let ours = |link_target: &Path| -> bool {
        link_target.parent() == Some(Path::new("../bin"))
            && link_target
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == zd_engine_core::LOADER_LINK_NAME || name == "zd-exec")
    };

    let wanted: std::collections::HashSet<&str> = binaries.iter().map(String::as_str).collect();

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
            if let Ok(link_target) = fs::read_link(&path)
                && ours(&link_target)
            {
                if let Err(e) = fs::remove_file(&path) {
                    log::warn!(
                        "runtime_links: failed to remove stale symlink {}: {e}",
                        path.display(),
                    );
                } else {
                    removed += 1;
                }
            }
        }
    }

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
                fs::remove_file(&link)
                    .with_context(|| format!("removing wrong-target symlink {}", link.display()))?;
            }
            Ok(_) => {
                skipped += 1;
                continue;
            }
            Err(_) => {}
        }

        std::os::unix::fs::symlink(target, &link)
            .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))?;
        created += 1;
    }

    log::info!(
        "runtime_links: zd-runtime ready at {} \
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

/// `ApplicationInfo.nativeLibraryDir` via JNI: where the installer put
/// this APK's `lib/arm64-v8a/*.so`. The path changes on every install
/// (`/data/app/~~<random>/com.zdroid-<random>/lib/arm64`), so nothing
/// persistent may embed it; look it up at boot.
pub fn native_library_dir(android_app: &AndroidApp) -> Result<PathBuf> {
    let dir = crate::jni_env::with_activity(android_app, |env, activity| {
        let value = env
            .call_method(activity, "nativeLibraryDir", "()Ljava/lang/String;", &[])
            .context("MainActivity.nativeLibraryDir")?;
        let jstr: JString = value
            .l()
            .context("nativeLibraryDir returned non-object")?
            .into();
        let dir: String = env
            .get_string(&jstr)
            .context("decode nativeLibraryDir")?
            .into();
        Ok(dir)
    })?;
    anyhow::ensure!(!dir.is_empty(), "nativeLibraryDir is empty");
    Ok(PathBuf::from(dir))
}

/// Absolute path of the rootfs-engine loader inside the native library
/// dir. The file exists only when the APK was packaged with
/// `extractNativeLibs="true"` (see `packaging.jniLibs` in build.gradle.kts).
pub fn loader_path(android_app: &AndroidApp) -> Result<PathBuf> {
    Ok(native_library_dir(android_app)?.join(zd_engine_core::LOADER_FILE_NAME))
}

/// Point `<data>/files/bin/zdload` at the current install's loader. The
/// kernel checks exec permission on the link's target, which stays an
/// `apk_data_file`.
pub fn ensure_loader_link(data_path: &Path, loader: &Path) -> Result<PathBuf> {
    let bin_dir = data_path.join("bin");
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("create_dir_all {}", bin_dir.display()))?;
    let link = bin_dir.join(zd_engine_core::LOADER_LINK_NAME);
    match fs::read_link(&link) {
        Ok(target) if target == loader => return Ok(link),
        Ok(_) => fs::remove_file(&link)
            .with_context(|| format!("remove stale loader link {}", link.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            // A real file squatting on the name: replace it, the name is ours.
            fs::remove_file(&link)
                .with_context(|| format!("remove non-link {}", link.display()))?;
        }
    }
    std::os::unix::fs::symlink(loader, &link)
        .with_context(|| format!("symlink {} -> {}", link.display(), loader.display()))?;
    log::info!(
        "runtime_links: loader link {} -> {}",
        link.display(),
        loader.display()
    );
    Ok(link)
}
