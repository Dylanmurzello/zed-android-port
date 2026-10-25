//! In-app updater for the Zdroid Android port. Hits the GitHub Releases
//! page for [`RELEASE_REPO`], compares the latest tag
//! against the running app's `versionName`, and (when newer) downloads
//! the signed APK to the app's cache dir and installs it through a
//! PackageInstaller session (`MainActivity.installUpdate`).
//!
//! Why ureq instead of reqwest: matches the existing
//! `bootstrap_install.rs` pattern (same crate transitively), avoids
//! pulling Tokio into a cold-start path that runs at most once per
//! session, and the synchronous shape is fine because we drive it from
//! a `cx.background_spawn` task that's already off the gpui thread.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use android_activity::AndroidApp;
use anyhow::{Context, Result, anyhow};
use futures::channel::oneshot;
use jni::{
    JNIEnv,
    objects::{JObject, JString, JValue},
    sys::jboolean,
};

use crate::credentials::with_activity;

/// Process-global AndroidApp handle. Set once at boot via
/// [`register_android_app`] by `gpui_android::run`; read by the public
/// updater functions so callers (lib.rs action handler, auto-check
/// task) don't have to thread an `AndroidApp` reference through their
/// own state.
static ANDROID_APP: OnceLock<AndroidApp> = OnceLock::new();

/// Stash the AndroidApp for later use by the updater. Idempotent (the
/// second set is silently dropped) so re-entry across Activity
/// recreations doesn't error.
pub fn register_android_app(app: AndroidApp) {
    let _ = ANDROID_APP.set(app);
}

pub(crate) fn android_app() -> Result<&'static AndroidApp> {
    ANDROID_APP
        .get()
        .ok_or_else(|| anyhow!("updater: AndroidApp not registered yet"))
}

/// Repo we ship APK releases from. Kept here (not in a config file)
/// so a malicious user-config tweak can't redirect the auto-updater
/// at a third-party APK. Forks that publish APKs signed with their own
/// key set `ZDROID_RELEASE_REPO` at build time: Android refuses to
/// install an update signed by a different key, so the updater has to
/// follow the repo that signed the running build.
pub const RELEASE_REPO: &str = match option_env!("ZDROID_RELEASE_REPO") {
    Some(repo) => repo,
    None => "Dylanmurzello/zed-android-port",
};

/// Reject unexpectedly large responses before they fill the app cache.
/// Current APKs are well below this.
const MAX_APK_BYTES: u64 = 512 * 1024 * 1024;

/// Asset name conventions the updater knows how to find. v0.2.0/v0.2.1
/// shipped with `app-release.apk` (gradle's default release-output
/// name); v0.2.2 onward switched to the prettier `Zdroid-X.Y.Z.apk`
/// for user-facing downloads but the updater code wasn't updated, so
/// the auto-update path silently 404'd between those versions. The
/// fix is to try both: the primary name follows the
/// `Zdroid-X.Y.Z.apk` convention so future releases just need the
/// pretty name, and `app-release.apk` stays as a fallback so a
/// stale release that uploaded only the gradle default still works.
/// Walked in order; first 200 wins.
fn candidate_asset_names(version: &str) -> [String; 2] {
    [
        format!("Zdroid-{version}.apk"),
        "app-release.apk".to_string(),
    ]
}

/// HTTP User-Agent. GitHub requires a non-empty UA for the static
/// redirect endpoint to behave correctly.
const USER_AGENT: &str = "zdroid-updater";

/// Where in the app cache dir we drop downloaded APKs.
const CACHE_SUBDIR: &str = "updater";

/// `PackageInstaller.STATUS_FAILURE_ABORTED`: the user declined Android's
/// install prompt.
const STATUS_FAILURE_ABORTED: i32 = 3;

/// Result of a "check for updates" call.
#[derive(Debug, Clone)]
pub enum UpdateCheck {
    /// Running app is already at the latest tag.
    UpToDate { current: String, latest: String },
    /// A newer tag exists. `download_urls` lists every URL the
    /// updater knows how to find the APK at, in priority order.
    /// `download_apk` walks the list and returns the first 200.
    /// The Vec is used (not a single URL) so a release that
    /// uploaded only one of our two known asset-name conventions
    /// still works.
    Available {
        current: String,
        latest: String,
        download_urls: Vec<String>,
    },
}

/// Resolve the latest release tag via the static redirect at
/// `https://github.com/<repo>/releases/latest`. Returns the tag (with
/// the leading `v` stripped if present). One HTTP request, no
/// `api.github.com` quota burn — same approach as
/// `bootstrap_install::resolve_latest_tag`.
pub fn fetch_latest_tag() -> Result<String> {
    let url = format!("https://github.com/{RELEASE_REPO}/releases/latest");
    let agent = ureq::builder().redirects(0).build();
    let resp = match agent.get(&url).set("User-Agent", USER_AGENT).call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(_, resp)) => resp,
        Err(e) => return Err(anyhow!("HTTP GET {url}: {e}")),
    };
    let location = resp.header("Location").ok_or_else(|| {
        anyhow!(
            "no Location header on {url}; got status {}",
            resp.status()
        )
    })?;
    let marker = "/releases/tag/";
    let after = location.find(marker).map(|i| &location[i + marker.len()..]);
    let tag = after
        .and_then(|s| s.split('/').next().filter(|t| !t.is_empty()))
        .ok_or_else(|| {
            anyhow!("expected `/releases/tag/<tag>` in Location {location}")
        })?;
    Ok(tag.trim_start_matches('v').to_owned())
}

/// Compare two semantic versions (both stripped of the leading `v`).
/// Returns `true` if `latest` is strictly newer than `current`.
/// Pre-release suffixes (`-pre`, `-rc.1`, etc.) sort after the bare
/// semver — we use the same lexicographic-after-split rule as upstream
/// semver crates so `0.2.0` < `0.2.1-pre` < `0.2.1`.
pub fn is_newer(current: &str, latest: &str) -> bool {
    fn parse(v: &str) -> (Vec<u64>, &str) {
        let (numeric, suffix) = match v.find('-') {
            Some(i) => (&v[..i], &v[i..]),
            None => (v, ""),
        };
        let parts = numeric
            .split('.')
            .map(|s| s.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>();
        (parts, suffix)
    }
    let (cur_n, cur_s) = parse(current);
    let (new_n, new_s) = parse(latest);
    if new_n != cur_n {
        return new_n > cur_n;
    }
    // Same numeric: stable (no suffix) > any pre-release; among pre-
    // releases, lexicographic.
    match (cur_s.is_empty(), new_s.is_empty()) {
        (true, true) => false,
        (true, false) => false,
        (false, true) => true,
        (false, false) => new_s > cur_s,
    }
}

/// Returns the currently-installed app `versionName` via JNI to
/// `MainActivity.appVersionName`. Empty string on JNI failure or when
/// PackageManager couldn't read its own metadata (both are paths the
/// caller should treat as "unknown — skip update flow").
pub fn current_version() -> String {
    let Ok(app) = android_app() else {
        return String::new();
    };
    match query_current_version(app) {
        Ok(v) => v,
        Err(err) => {
            log::warn!("updater: current_version JNI failed: {err:#}");
            String::new()
        }
    }
}

fn query_current_version(android_app: &AndroidApp) -> Result<String> {
    with_activity(android_app, |env, activity| {
        let result = env.call_method(activity, "appVersionName", "()Ljava/lang/String;", &[])?;
        let s: JString = result.l()?.into();
        Ok(env.get_string(&s)?.into())
    })
}

/// `Context.getCacheDir().getAbsolutePath()` via JNI. The
/// `AndroidApp::internal_data_path()` accessor on android-activity 0.6
/// only exposes the FILES dir; there is no cache-dir accessor, so we
/// reach through to the Activity (which inherits the Context method).
fn system_cache_dir(android_app: &AndroidApp) -> Result<PathBuf> {
    with_activity(android_app, |env, activity| {
        let file = env
            .call_method(activity, "getCacheDir", "()Ljava/io/File;", &[])?
            .l()?;
        let path_value = env.call_method(&file, "getAbsolutePath", "()Ljava/lang/String;", &[])?;
        let s: JString = path_value.l()?.into();
        let path: String = env.get_string(&s)?.into();
        Ok(PathBuf::from(path))
    })
}

/// Top-level check-for-update entry. Resolves the latest tag, compares
/// against the running version, and returns a structured result. Does
/// no downloading.
pub fn check_for_update() -> Result<UpdateCheck> {
    let latest = fetch_latest_tag().context("resolve latest release tag")?;
    let current = current_version();
    log::info!(
        "updater: check_for_update current={current:?} latest={latest:?}"
    );
    if current.is_empty() {
        return Err(anyhow!(
            "can't determine current app version; PackageManager returned empty"
        ));
    }
    if is_newer(&current, &latest) {
        // Build candidate URLs in priority order. `download_apk` will
        // walk them and use the first one that returns 200. The
        // primary slot stays the prettier `Zdroid-X.Y.Z.apk` name
        // that v0.2.2+ release notes refer to; the secondary slot is
        // gradle's `app-release.apk` for releases that only uploaded
        // that name.
        let download_urls = candidate_asset_names(&latest)
            .into_iter()
            .map(|name| {
                format!(
                    "https://github.com/{RELEASE_REPO}/releases/download/v{latest}/{name}"
                )
            })
            .collect();
        Ok(UpdateCheck::Available {
            current,
            latest,
            download_urls,
        })
    } else {
        Ok(UpdateCheck::UpToDate { current, latest })
    }
}

/// Download the release APK to `<cacheDir>/updater/zdroid-<tag>.apk`.
/// Walks `urls` in order, returning as soon as one returns 200. A 404
/// (or any other non-2xx) is logged and the next URL is tried; this
/// is how the updater absorbs asset-name drift between releases
/// (see [`candidate_asset_names`] for the why). Returns an error only
/// when ALL candidate URLs have failed. Streams the response body so
/// we don't hold the full ~225 MB APK in memory. `on_progress` gets the
/// downloaded fraction each time the whole percentage changes, or `None`
/// when the server sends no length.
pub fn download_apk(
    tag: &str,
    urls: &[String],
    mut on_progress: impl FnMut(Option<f32>),
) -> Result<PathBuf> {
    if urls.is_empty() {
        return Err(anyhow!("download_apk: no candidate URLs"));
    }
    let android_app = android_app()?;
    // Use Context.getCacheDir() via JNI. `android_app.internal_data_path()`
    // returns the FILES dir (`getFilesDir()`); the OS cache dir is its
    // sibling, NOT a `cache/` subdirectory of files. Pre-v0.3.1 the
    // updater wrote to `<filesDir>/cache/updater/`. One-time migration:
    // if that directory exists from a previous attempt, wipe it so stale
    // 225 MB APKs don't sit on disk forever.
    let cache_root = system_cache_dir(android_app)
        .context("query Context.getCacheDir() via JNI")?;
    let stale_dir = android_app
        .internal_data_path()
        .map(|p| p.join("cache").join(CACHE_SUBDIR));
    if let Some(stale) = stale_dir.as_ref()
        && stale.exists()
        && stale != &cache_root.join(CACHE_SUBDIR)
    {
        if let Err(err) = std::fs::remove_dir_all(stale) {
            log::warn!(
                "updater: failed to wipe stale cache dir {}: {err:#}",
                stale.display()
            );
        } else {
            log::info!("updater: wiped stale cache dir {}", stale.display());
        }
    }
    let cache_dir = cache_root.join(CACHE_SUBDIR);
    std::fs::create_dir_all(&cache_dir).context("create updater cache dir")?;
    let dest = cache_dir.join(format!("zdroid-{tag}.apk"));

    let agent = ureq::builder().redirects(5).build();
    let mut last_err: Option<anyhow::Error> = None;
    for url in urls {
        // Wipe any partial download from a prior failed candidate so
        // a 404 followed by a 200 doesn't leave a truncated file from
        // the 404 attempt under the same dest path.
        if dest.exists() {
            let _ = std::fs::remove_file(&dest);
        }
        log::info!("updater: downloading {url} -> {}", dest.display());
        let resp = match agent
            .get(url)
            .set("User-Agent", USER_AGENT)
            .call()
        {
            Ok(resp) => resp,
            Err(err) => {
                log::warn!("updater: GET {url} failed: {err:#}; trying next candidate");
                last_err = Some(anyhow::Error::new(err).context(format!("GET {url}")));
                continue;
            }
        };
        let total_bytes = resp
            .header("Content-Length")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|length| *length > 0);
        if let Some(length) = total_bytes
            && length > MAX_APK_BYTES
        {
            last_err = Some(anyhow!(
                "update APK is {length} bytes, above the {MAX_APK_BYTES}-byte safety limit"
            ));
            continue;
        }
        on_progress(total_bytes.map(|_| 0.0));
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(&dest)
            .with_context(|| format!("create {}", dest.display()))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        let mut last_reported_percent = None;
        let read_result = (|| -> Result<()> {
            loop {
                let n = reader.read(&mut buf).context("read response body")?;
                if n == 0 {
                    break;
                }
                std::io::Write::write_all(&mut file, &buf[..n])
                    .with_context(|| format!("write {}", dest.display()))?;
                total += n as u64;
                if total > MAX_APK_BYTES {
                    return Err(anyhow!(
                        "update APK exceeded the {MAX_APK_BYTES}-byte safety limit"
                    ));
                }
                if let Some(total_bytes) = total_bytes {
                    let fraction = (total as f32 / total_bytes as f32).clamp(0.0, 1.0);
                    let percent = (fraction * 100.0) as u8;
                    if last_reported_percent != Some(percent) {
                        last_reported_percent = Some(percent);
                        on_progress(Some(fraction));
                    }
                }
            }
            Ok(())
        })();
        drop(file);
        match read_result {
            Ok(()) => {
                log::info!("updater: downloaded {total} bytes to {}", dest.display());
                return Ok(dest);
            }
            Err(err) => {
                log::warn!("updater: streaming {url} failed: {err:#}; trying next candidate");
                if let Err(remove_err) = std::fs::remove_file(&dest) {
                    log::warn!("updater: removing partial {} failed: {remove_err}", dest.display());
                }
                last_err = Some(err);
                continue;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow!("download_apk: all candidate URLs failed without errors")))
}

/// Delete the APKs `download_apk` left in the app cache.
pub fn remove_downloaded_apks() -> Result<()> {
    let cache_dir = system_cache_dir(android_app()?)?.join(CACHE_SUBDIR);
    match std::fs::remove_dir_all(&cache_dir) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(anyhow::Error::new(err).context(format!("remove {}", cache_dir.display())))
        }
        _ => Ok(()),
    }
}

/// How an install session ended, when it didn't replace the app.
#[derive(Debug)]
pub enum InstallOutcome {
    /// The user declined Android's install prompt.
    Cancelled,
    Failed(String),
}

static INSTALL_RESULT: Mutex<Option<oneshot::Sender<InstallOutcome>>> = Mutex::new(None);
static INSTALL_PERMISSION_RESULT: Mutex<Option<oneshot::Sender<bool>>> = Mutex::new(None);

fn replace_sender<T>(slot: &Mutex<Option<oneshot::Sender<T>>>, sender: Option<oneshot::Sender<T>>) {
    match slot.lock() {
        Ok(mut guard) => *guard = sender,
        Err(poisoned) => *poisoned.into_inner() = sender,
    }
}

fn take_sender<T>(slot: &Mutex<Option<oneshot::Sender<T>>>) -> Option<oneshot::Sender<T>> {
    match slot.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

/// Whether Android lets Zdroid install APKs ("Install unknown apps").
pub fn can_install_updates() -> Result<bool> {
    with_activity(android_app()?, |env, activity| {
        Ok(env
            .call_method(activity, "canInstallUpdates", "()Z", &[])?
            .z()?)
    })
}

/// Opens Zdroid's "Install unknown apps" settings page. The receiver
/// resolves with whether the user allowed it once they come back.
pub fn request_install_permission() -> Result<oneshot::Receiver<bool>> {
    let android_app = android_app()?;
    let (sender, receiver) = oneshot::channel();
    replace_sender(&INSTALL_PERMISSION_RESULT, Some(sender));
    let result = with_activity(android_app, |env, activity| {
        env.call_method(activity, "requestInstallPermission", "()V", &[])?;
        Ok(())
    });
    if let Err(error) = result {
        replace_sender(&INSTALL_PERMISSION_RESULT, None);
        return Err(error);
    }
    Ok(receiver)
}

/// Installs the downloaded APK through a PackageInstaller session, which
/// asks the user to confirm and then replaces the running app. Blocks
/// while the APK is copied into the session, so call it off the gpui
/// thread. The receiver resolves only if the session ends without
/// installing; on success Android kills the app instead.
pub fn install_update(apk_path: &Path) -> Result<oneshot::Receiver<InstallOutcome>> {
    let path = apk_path
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF8 APK path: {}", apk_path.display()))?;
    let android_app = android_app()?;
    let (sender, receiver) = oneshot::channel();
    replace_sender(&INSTALL_RESULT, Some(sender));
    let result = with_activity(android_app, |env, activity| {
        let path = env.new_string(path)?;
        let error = env
            .call_method(
                activity,
                "installUpdate",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&path.into())],
            )?
            .l()?;
        if error.is_null() {
            return Ok(());
        }
        let error: String = env.get_string(&JString::from(error))?.into();
        Err(anyhow!("{error}"))
    });
    if let Err(error) = result {
        replace_sender(&INSTALL_RESULT, None);
        return Err(error);
    }
    Ok(receiver)
}

/// Shows Android's prompt for the install session in flight again, in
/// case it ended up behind other apps. Returns false if Android hasn't
/// asked yet.
pub fn show_install_prompt() -> Result<bool> {
    with_activity(android_app()?, |env, activity| {
        Ok(env
            .call_method(activity, "showUpdatePrompt", "()Z", &[])?
            .z()?)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnUpdateInstallResult<'local>(
    mut env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    status: i32,
    message: JString<'local>,
) {
    let message: Option<String> = if message.is_null() {
        None
    } else {
        env.get_string(&message).ok().map(Into::into)
    };
    log::info!("updater: install session ended with status {status} ({message:?})");
    let outcome = if status == STATUS_FAILURE_ABORTED {
        InstallOutcome::Cancelled
    } else {
        InstallOutcome::Failed(
            message.unwrap_or_else(|| format!("Android's installer failed with status {status}")),
        )
    };
    if let Some(sender) = take_sender(&INSTALL_RESULT) {
        sender.send(outcome).ok();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnInstallPermissionResult<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    granted: jboolean,
) {
    if let Some(sender) = take_sender(&INSTALL_PERMISSION_RESULT) {
        sender.send(granted != 0).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.2.1"));
        assert!(is_newer("0.2.0", "0.3.0"));
        assert!(is_newer("0.2.0", "1.0.0"));
        assert!(!is_newer("0.2.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        // Pre-release ordering: 0.2.0 < 0.2.1-pre < 0.2.1
        assert!(is_newer("0.2.0", "0.2.1-pre"));
        assert!(is_newer("0.2.1-pre", "0.2.1"));
        assert!(!is_newer("0.2.1", "0.2.1-pre"));
    }
}
