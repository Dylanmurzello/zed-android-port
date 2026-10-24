//! Download + extract path for [`super::bootstrap::BootstrapAdapter::install`].
//!
//! Pulls the latest release tarball from `<release_repo>`'s GitHub
//! releases endpoint, downloads `bootstrap-aarch64.zip`, extracts the
//! contents into a staging dir, swaps the staging into `$PREFIX`
//! atomically, and writes a version sentinel so subsequent boots skip
//! re-extraction.
//!
//! Lives in `zdroid_runtime` rather than `gpui_android` so the adapter
//! is self-contained — the historical extraction code in
//! `gpui_android::termux_bootstrap` couples to APK assets and the older
//! bundled-zip flow. Phase 8 of the Termux-divestment refactor sweeps
//! the legacy module; this is the replacement for the install path.

use std::fs;
use std::io::{Cursor, Read, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::health::ProgressSink;

/// Preferred asset name inside the GitHub release. The
/// bootstrap-build pipeline names every release's archive identically;
/// the version is encoded in the release TAG, not the asset filename.
///
/// On 404 we fall back to enumerating the release's assets via the
/// GitHub API and picking the first whose name matches
/// `ASSET_NAME_REGEX_PATTERN` (eg. `bootstrap-aarch64-r4.zip`). The
/// fallback path is robust to typos at upload time; the canonical
/// name keeps the steady-state path off `api.github.com`.
const RELEASE_ASSET_NAME: &str = "bootstrap-aarch64.zip";

/// Prefix + extension that any acceptable bootstrap asset must match.
/// The fallback asset-enumeration path picks the first asset whose
/// name starts with this prefix and ends with `.zip`. Covers
/// `bootstrap-aarch64.zip`, `bootstrap-aarch64-zdroid.zip`,
/// `bootstrap-aarch64-r4.zip`, etc.
const ASSET_NAME_PREFIX: &str = "bootstrap-aarch64";
const ASSET_NAME_SUFFIX: &str = ".zip";

/// Manifest entry inside the bootstrap zip carrying symlink targets
/// the zip format itself can't represent on extraction (Android's
/// extract path doesn't preserve mode bits + symlinks the way `unzip`
/// does on a real Unix). Replayed after the regular extract pass.
/// Upper bounds that keep a corrupt or hostile archive from exhausting
/// memory or storage. The real bootstrap is ~250 MB with ~20k entries.
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 300_000;

const SYMLINKS_ENTRY: &str = "SYMLINKS.txt";
const SYMLINKS_DELIM: &str = "←";

/// File at `$PREFIX/.bootstrap-version` recording which release tag is
/// currently extracted. Re-extracts only fire when this doesn't match
/// the latest release tag at download time.
const VERSION_FILE: &str = ".bootstrap-version";

/// Download the latest release zip + extract into `<prefix>` atomically.
/// Idempotent against `<prefix>/.bootstrap-version` — if the on-disk
/// sentinel matches the latest tag, the function returns Ok without
/// touching the network or filesystem beyond a tag resolve.
pub fn install_latest(
    prefix: &Path,
    release_repo: &str,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    progress.step("Resolving latest bootstrap release");
    let tag_name = resolve_latest_tag(release_repo)
        .with_context(|| format!("resolve latest release tag for {release_repo}"))?;
    log::info!("bootstrap_install: latest release tag = {tag_name}");

    let version_file = prefix.join(VERSION_FILE);
    if let Ok(existing) = fs::read_to_string(&version_file)
        && existing.trim() == tag_name
    {
        log::info!(
            "bootstrap_install: $PREFIX already at {tag_name}, skipping extract"
        );
        progress.step(&format!("Bootstrap {tag_name} already installed"));
        return Ok(());
    }

    progress.step(&format!("Downloading bootstrap {tag_name}"));
    let zip_bytes = download_bootstrap_asset(release_repo, &tag_name).with_context(|| {
        format!("download bootstrap asset for {release_repo} tag {tag_name}")
    })?;
    log::info!(
        "bootstrap_install: downloaded {} bytes for tag {}",
        zip_bytes.len(),
        tag_name,
    );

    progress.step("Extracting bootstrap");
    let staging = prefix.with_extension("staging");
    extract_into_staging(&zip_bytes, &staging)
        .with_context(|| format!("extract into {}", staging.display()))?;

    swap_staging_into_prefix(&staging, prefix)
        .with_context(|| format!("swap staging into {}", prefix.display()))?;

    if let Some(parent) = version_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::File::create(&version_file)
        .with_context(|| format!("create version sentinel at {}", version_file.display()))?
        .write_all(tag_name.as_bytes())?;

    log::info!(
        "bootstrap_install: bootstrap {} ready at {}",
        tag_name,
        prefix.display()
    );
    progress.step(&format!("Bootstrap {tag_name} installed"));
    Ok(())
}

/// Resolve the latest-release tag without naming any asset.
///
/// Hits `https://github.com/<repo>/releases/latest` with redirects
/// disabled. GitHub returns a 302 whose `Location` is
/// `https://github.com/<repo>/releases/tag/<tag>`. Parse the tag out.
/// One HTTP request, no `api.github.com`, no asset name dependency —
/// works even when the release has zero assets uploaded.
fn resolve_latest_tag(release_repo: &str) -> Result<String> {
    let url = format!("https://github.com/{release_repo}/releases/latest");
    let agent_no_redirect = ureq::builder().redirects(0).build();
    let head = agent_no_redirect
        .get(&url)
        .set("User-Agent", "zdroid-bootstrap-installer")
        .call();
    let resp = match head {
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
    Ok(tag.to_owned())
}

/// Download the bootstrap zip for the given tag, verifying it when possible.
///
/// Enumerates the release's assets via
/// `api.github.com/repos/<repo>/releases/tags/<tag>` to pick the canonical
/// `RELEASE_ASSET_NAME`, or else the first
/// `<ASSET_NAME_PREFIX>*<ASSET_NAME_SUFFIX>` asset, along with the size and
/// SHA-256 digest GitHub computed at upload; a download that doesn't match
/// is rejected. The API allows 60 anonymous requests an hour per IP, which
/// carrier NAT can use up, so when it's unavailable the canonical asset is
/// downloaded unverified rather than failing the install.
fn download_bootstrap_asset(release_repo: &str, tag: &str) -> Result<Vec<u8>> {
    let asset = find_release_asset(release_repo, tag)
        .map_err(|error| {
            log::warn!(
                "bootstrap_install: release metadata unavailable, downloading \
                 {RELEASE_ASSET_NAME} without verification: {error:#}"
            );
        })
        .ok();
    if let Some(asset) = &asset
        && asset.size > MAX_ARCHIVE_BYTES
    {
        return Err(anyhow!(
            "bootstrap asset {} is {} bytes, above the {MAX_ARCHIVE_BYTES}-byte safety limit",
            asset.name,
            asset.size,
        ));
    }
    let name = asset
        .as_ref()
        .map_or(RELEASE_ASSET_NAME, |asset| asset.name.as_str());
    let url = format!("https://github.com/{release_repo}/releases/download/{tag}/{name}");
    log::info!("bootstrap_install: fetching {name}");
    let bytes = fetch_asset_bytes(&url)?;
    if let Some(asset) = &asset {
        validate_bootstrap_asset(&bytes, asset)?;
    }
    Ok(bytes)
}

fn validate_bootstrap_asset(bytes: &[u8], asset: &GitHubAsset) -> Result<()> {
    if bytes.len() as u64 != asset.size {
        return Err(anyhow!(
            "bootstrap asset size mismatch: downloaded {}, GitHub reports {}",
            bytes.len(),
            asset.size
        ));
    }
    // GitHub only records digests for assets uploaded since mid-2025.
    let Some(expected) = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
    else {
        log::warn!(
            "bootstrap_install: GitHub has no SHA-256 digest for {}; checked size only",
            asset.name
        );
        return Ok(());
    };
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(anyhow!("bootstrap asset SHA-256 mismatch for {}", asset.name));
    }
    log::info!(
        "bootstrap_install: verified {} ({} bytes, sha256:{actual})",
        asset.name,
        asset.size,
    );
    Ok(())
}

fn fetch_asset_bytes(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .set("User-Agent", "zdroid-bootstrap-installer")
        .call()
        .map_err(|e| anyhow!("HTTP GET {url}: {e}"))?;
    let cap = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if cap > MAX_ARCHIVE_BYTES {
        return Err(anyhow!(
            "bootstrap response is {cap} bytes, above the {MAX_ARCHIVE_BYTES}-byte safety limit"
        ));
    }
    let mut buf = Vec::with_capacity(cap as usize);
    // Read one byte past the limit so an oversized body is detected
    // without buffering all of it.
    resp.into_reader()
        .take(MAX_ARCHIVE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| anyhow!("read body from {url}: {e}"))?;
    if buf.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(anyhow!(
            "bootstrap response exceeded the {MAX_ARCHIVE_BYTES}-byte safety limit"
        ));
    }
    Ok(buf)
}

#[derive(Deserialize)]
struct GitHubRelease {
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    size: u64,
    digest: Option<String>,
}

fn find_release_asset(release_repo: &str, tag: &str) -> Result<GitHubAsset> {
    let url = format!("https://api.github.com/repos/{release_repo}/releases/tags/{tag}");
    let release: GitHubRelease = ureq::get(&url)
        .set("User-Agent", "zdroid-bootstrap-installer")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| anyhow!("HTTP GET {url}: {e}"))?
        .into_json()
        .map_err(|e| anyhow!("parse release metadata from {url}: {e}"))?;
    let index = release
        .assets
        .iter()
        .position(|asset| asset.name == RELEASE_ASSET_NAME)
        .or_else(|| {
            release.assets.iter().position(|asset| {
                asset.name.starts_with(ASSET_NAME_PREFIX) && asset.name.ends_with(ASSET_NAME_SUFFIX)
            })
        })
        .ok_or_else(|| {
            anyhow!("release {tag} has no asset matching {ASSET_NAME_PREFIX}*{ASSET_NAME_SUFFIX}")
        })?;
    let mut assets = release.assets;
    Ok(assets.swap_remove(index))
}

fn extract_into_staging(zip_bytes: &[u8], staging: &Path) -> Result<()> {
    if staging.exists() {
        fs::remove_dir_all(staging)
            .with_context(|| format!("wipe leftover staging at {}", staging.display()))?;
    }
    fs::create_dir_all(staging)
        .with_context(|| format!("create staging dir {}", staging.display()))?;

    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .context("ZipArchive::new on downloaded bootstrap")?;

    let symlinks = extract_entries(&mut archive, staging)?;
    log::info!(
        "bootstrap_install: extracted {} entries, {} symlinks queued",
        archive.len(),
        symlinks.len(),
    );
    replay_symlinks(staging, &symlinks)?;
    Ok(())
}

fn swap_staging_into_prefix(staging: &Path, prefix: &Path) -> Result<()> {
    if prefix.exists() {
        fs::remove_dir_all(prefix)
            .with_context(|| format!("wipe old prefix at {}", prefix.display()))?;
    }
    fs::rename(staging, prefix).with_context(|| {
        format!("rename {} -> {}", staging.display(), prefix.display())
    })?;
    Ok(())
}

fn extract_entries<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    staging: &Path,
) -> Result<Vec<(String, String)>> {
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(anyhow!(
            "bootstrap archive has {} entries, above the {MAX_ARCHIVE_ENTRIES}-entry safety limit",
            archive.len(),
        ));
    }
    let mut symlinks = Vec::new();
    let mut extracted_bytes = 0_u64;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let raw_name = entry.name().to_owned();
        extracted_bytes = extracted_bytes
            .checked_add(entry.size())
            .ok_or_else(|| anyhow!("bootstrap extracted-size counter overflow"))?;
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            return Err(anyhow!(
                "bootstrap archive expands beyond the {MAX_EXTRACTED_BYTES}-byte safety limit"
            ));
        }

        if raw_name == SYMLINKS_ENTRY {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }
                let Some((target, link_rel)) = line.split_once(SYMLINKS_DELIM) else {
                    log::warn!("bootstrap_install: malformed SYMLINKS.txt line: {line:?}");
                    continue;
                };
                symlinks.push((target.to_owned(), link_rel.to_owned()));
            }
            continue;
        }

        let Some(safe) = entry.enclosed_name() else {
            log::warn!("bootstrap_install: skipping unsafe entry path {raw_name:?}");
            continue;
        };
        let dest: PathBuf = staging.join(&safe);

        if entry.is_dir() {
            fs::create_dir_all(&dest)?;
            continue;
        }

        if entry.is_symlink() {
            log::warn!(
                "bootstrap_install: unexpected inline symlink entry {raw_name:?}; \
                 skipping (symlinks come via SYMLINKS.txt)"
            );
            continue;
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let entry_mode = entry.unix_mode();
        let mut out =
            fs::File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)?;

        if let Some(mode) = entry_mode {
            let owner_only =
                (mode & 0o700) | if mode & 0o100 != 0 { 0o700 } else { 0o600 };
            let mut perms = fs::metadata(&dest)?.permissions();
            perms.set_mode(owner_only);
            fs::set_permissions(&dest, perms)?;
        } else if raw_name.starts_with("bin/")
            || raw_name.starts_with("libexec/")
            || raw_name.starts_with("lib/apt/methods/")
            || raw_name == "lib/apt/apt-helper"
        {
            let mut perms = fs::metadata(&dest)?.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&dest, perms)?;
        }
    }

    Ok(symlinks)
}

fn replay_symlinks(staging: &Path, symlinks: &[(String, String)]) -> Result<()> {
    for (target, link_rel) in symlinks {
        // Regular entries go through `enclosed_name`; symlink paths come from
        // SYMLINKS.txt and need the same containment check.
        let link_rel = safe_symlink_path(link_rel)?;
        reject_symlink_ancestors(staging, &link_rel)?;
        let link_abs = staging.join(&link_rel);
        if let Some(parent) = link_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        if link_abs.exists() || link_abs.symlink_metadata().is_ok() {
            fs::remove_file(&link_abs).ok();
        }
        std::os::unix::fs::symlink(target, &link_abs)
            .with_context(|| format!("symlink {} -> {}", link_abs.display(), target))?;
    }
    Ok(())
}

fn safe_symlink_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    let has_normal_component = path
        .components()
        .any(|component| matches!(component, Component::Normal(_)));
    if !has_normal_component
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(anyhow!("unsafe bootstrap symlink path {raw:?}"));
    }
    Ok(path.to_path_buf())
}

/// Refuses to create a symlink through an earlier symlink, which could
/// otherwise redirect it outside the staging directory.
fn reject_symlink_ancestors(staging: &Path, relative: &Path) -> Result<()> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = staging.to_path_buf();
    for component in parent.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(anyhow!(
                    "bootstrap symlink path traverses another symlink at {}",
                    current.display()
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(anyhow!(
                    "bootstrap symlink parent is not a directory: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .with_context(|| format!("create symlink parent {}", current.display()))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// `ProgressSink` impl that routes progress events to logcat. Useful
/// when the caller is happy with log-only feedback (e.g. headless
/// first-boot install before any UI is up to render a progress bar).
#[derive(Debug)]
pub struct LogProgressSink;

impl ProgressSink for LogProgressSink {
    fn step(&mut self, label: &str) {
        log::info!("bootstrap_install: {label}");
    }
    fn progress(&mut self, done: u64, total: u64) {
        if total > 0 {
            log::debug!("bootstrap_install: {done}/{total}");
        }
    }
    fn warn(&mut self, message: &str) {
        log::warn!("bootstrap_install: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_paths_must_stay_inside_staging() {
        assert!(safe_symlink_path("./usr/bin/sh").is_ok());
        assert!(safe_symlink_path("usr/lib/libz.so").is_ok());
        for path in ["", ".", "/etc/passwd", "../outside", "usr/../../outside"] {
            assert!(safe_symlink_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn symlinks_cannot_be_created_through_other_symlinks() -> Result<()> {
        let staging = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(outside.path(), staging.path().join("usr"))?;

        let symlinks = vec![("target".to_owned(), "./usr/bin/link".to_owned())];
        assert!(replay_symlinks(staging.path(), &symlinks).is_err());
        assert!(!outside.path().join("bin").exists());
        Ok(())
    }

    #[test]
    fn bootstrap_asset_must_match_size_and_digest() {
        let asset = |size, digest: &str| GitHubAsset {
            name: RELEASE_ASSET_NAME.to_owned(),
            size,
            digest: Some(digest.to_owned()),
        };
        let abc_digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        assert!(validate_bootstrap_asset(b"abc", &asset(3, abc_digest)).is_ok());
        assert!(validate_bootstrap_asset(b"abd", &asset(3, abc_digest)).is_err());
        assert!(validate_bootstrap_asset(b"abc", &asset(4, abc_digest)).is_err());
        let mut no_digest = asset(3, abc_digest);
        no_digest.digest = None;
        assert!(validate_bootstrap_asset(b"abc", &no_digest).is_ok());
        assert!(validate_bootstrap_asset(b"abcd", &no_digest).is_err());
    }
}
