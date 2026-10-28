//! Download, verify, extract and activate the Ubuntu base image.
//!
//! The extractor is deliberately not `tar::Archive::unpack`: every
//! symlink is rewritten so the kernel can resolve it from the host side
//! (the engine's relative-symlink invariant), device nodes and fifos are
//! skipped because `/dev` is bound from the host, ownership is ignored
//! (everything belongs to the app uid; the loader presents uid 0), and
//! modes are kept so setuid bits and executable bits survive. The new
//! tree is built next to the live one and swapped in with two renames.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::{Archive, EntryType};
use zd_engine_core::LIST_SEPARATOR;
use zd_engine_core::bindings::Bindings;
use zd_engine_core::path::MAX_PATH;
use zd_engine_core::translate::Translator;

use super::rootfs::{RootfsAdapter, VERSION_FILE};
use crate::health::ProgressSink;

/// The install stopped because the sink reported cancellation. Callers
/// downcast to tell it apart from failures.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("install cancelled")
    }
}

impl std::error::Error for Cancelled {}

const MIN_FREE_BYTES: u64 = 600 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const USER_AGENT: &str = "zdroid-rootfs-installer";
const ESSENTIALS_SCRIPT: &str =
    "apt-get update && apt-get install -y --no-install-recommends ca-certificates git";
const ESSENTIALS_HINT: &str = "run `apt-get install -y ca-certificates git` in the terminal";

fn sibling(rootfs: &Path, suffix: &str) -> PathBuf {
    let mut name = rootfs
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "rootfs".into());
    name.push(".");
    name.push(suffix);
    rootfs.with_file_name(name)
}

pub fn install(
    adapter: &RootfsAdapter,
    nameservers: &[String],
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    let rootfs = adapter.rootfs_dir();
    let staging = sibling(rootfs, "staging");
    let download = sibling(rootfs, "download");
    let old = sibling(rootfs, "old");

    preflight(adapter, &[&staging, &download, &old])?;
    let result = install_inner(adapter, nameservers, progress, &staging, &download, &old);
    if result.is_err() {
        sweep(&staging);
        sweep(&download);
    }
    result
}

pub fn uninstall(adapter: &RootfsAdapter) -> Result<()> {
    let rootfs = adapter.rootfs_dir();
    sweep(&sibling(rootfs, "staging"));
    sweep(&sibling(rootfs, "download"));
    if rootfs.exists() {
        let old = sibling(rootfs, "old");
        sweep(&old);
        fs::rename(rootfs, &old)
            .with_context(|| format!("rename {} -> {}", rootfs.display(), old.display()))?;
        fs::remove_dir_all(&old).with_context(|| format!("remove {}", old.display()))?;
    }
    Ok(())
}

fn preflight(adapter: &RootfsAdapter, stale: &[&Path]) -> Result<()> {
    let loader = adapter.loader_path();
    if !loader.exists() {
        bail!(
            "engine loader missing at {} (reinstall the app)",
            loader.display()
        );
    }
    let parent = adapter
        .rootfs_dir()
        .parent()
        .context("rootfs_dir has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    if let Some(free) = free_bytes(parent)
        && free < MIN_FREE_BYTES
    {
        bail!(
            "not enough free space: {} MB available, {} MB needed",
            free / (1024 * 1024),
            MIN_FREE_BYTES / (1024 * 1024)
        );
    }
    for path in stale {
        sweep(path);
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
    if rc != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(target_os = "android"))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

fn sweep(path: &Path) {
    if !path.exists() {
        return;
    }
    match fs::remove_dir_all(path) {
        Ok(()) => log::info!("rootfs_install: removed {}", path.display()),
        Err(error) => log::warn!("rootfs_install: could not remove {}: {error}", path.display()),
    }
}

fn check_cancelled(progress: &dyn ProgressSink) -> Result<()> {
    if progress.is_cancelled() {
        return Err(Cancelled.into());
    }
    Ok(())
}

fn install_inner(
    adapter: &RootfsAdapter,
    nameservers: &[String],
    progress: &mut dyn ProgressSink,
    staging: &Path,
    download: &Path,
    old: &Path,
) -> Result<()> {
    let config = adapter.config();
    let agent = ureq::AgentBuilder::new()
        .redirects(5)
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();

    progress.step("Fetching checksums");
    let expected = fetch_expected_sha256(&agent, &config.sha256sums_url, &config.image_name)?;
    check_cancelled(progress)?;

    progress.step(&format!("Downloading {}", config.image_name));
    fs::create_dir_all(download).with_context(|| format!("create {}", download.display()))?;
    let tarball = download.join(&config.image_name);
    let actual = download_file(&agent, &config.image_url, &tarball, progress)?;
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            config.image_name
        );
    }

    progress.step("Extracting");
    let translator_rootfs = adapter.rootfs_dir().as_os_str().as_bytes().to_vec();
    let bind_spec = adapter.bind_spec().into_bytes();
    let mut bindings = Bindings::with_defaults();
    bindings
        .parse_spec(&bind_spec)
        .map_err(|error| anyhow::anyhow!("bind spec: {error:?}"))?;
    let translator = Translator::new(&translator_rootfs, bindings);
    extract(&tarball, staging, &translator, progress)?;

    progress.step("Seeding configuration");
    let bind_guest_dirs = adapter
        .bind_spec()
        .split(LIST_SEPARATOR)
        .map(|entry| entry.split_once(':').map_or(entry, |(guest, _host)| guest).to_string())
        .collect::<Vec<_>>();
    seed(staging, nameservers, &bind_guest_dirs)?;
    fs::write(
        staging.join(VERSION_FILE),
        format!("{}\n{actual}\n", config.image_name),
    )
    .context("write version file")?;

    progress.step("Activating");
    let rootfs = adapter.rootfs_dir();
    if rootfs.exists() {
        fs::rename(rootfs, old)
            .with_context(|| format!("rename {} -> {}", rootfs.display(), old.display()))?;
    }
    fs::rename(staging, rootfs)
        .with_context(|| format!("rename {} -> {}", staging.display(), rootfs.display()))?;
    sweep(old);
    sweep(download);
    log::info!(
        "rootfs_install: {} ({actual}) active at {}",
        config.image_name,
        rootfs.display()
    );

    progress.step("Installing essentials (ca-certificates, git)");
    if let Err(error) = install_essentials(adapter, progress) {
        progress.warn(&format!(
            "Essentials were not installed ({error:#}); {ESSENTIALS_HINT}."
        ));
    }
    Ok(())
}

/// `ubuntu-base` ships neither git nor CA certificates, so this runs
/// apt through the loader right after activation. Non-fatal: the rootfs
/// is complete without it, and the terminal hint covers the rest.
// Synchronous on purpose: the installer already runs on its own thread
// and the adapter spawns the loader directly (see `RootfsAdapter::spawn`).
#[allow(clippy::disallowed_methods)]
fn install_essentials(adapter: &RootfsAdapter, progress: &mut dyn ProgressSink) -> Result<()> {
    let mut command = adapter.command("/bin/sh");
    command
        .args(["-c", ESSENTIALS_SCRIPT])
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("spawn apt-get through the loader")?;
    let stdout = child.stdout.take().context("apt-get stdout")?;
    let stderr = child.stderr.take().context("apt-get stderr")?;
    let (sender, receiver) = mpsc::channel::<String>();
    let output_reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let error_reader = thread::spawn(move || {
        let mut text = String::new();
        if let Err(error) = BufReader::new(stderr).read_to_string(&mut text) {
            log::warn!("rootfs_install: reading apt-get stderr: {error}");
        }
        text
    });
    let mut last_line = String::new();
    let mut cancelled = false;
    loop {
        match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                let line = line.trim();
                if !line.is_empty() {
                    last_line = line.to_string();
                    progress.step(&format!("Installing essentials: {}", shorten(line, 72)));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if progress.is_cancelled() {
                    cancelled = true;
                    child.kill().context("kill apt-get")?;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().context("wait for apt-get")?;
    output_reader
        .join()
        .map_err(|_| anyhow::anyhow!("apt-get stdout reader panicked"))?;
    let errors = error_reader
        .join()
        .map_err(|_| anyhow::anyhow!("apt-get stderr reader panicked"))?;
    if cancelled {
        bail!("cancelled");
    }
    if !status.success() {
        let detail = errors
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(last_line.as_str());
        bail!("apt-get exited with {status}: {detail}");
    }
    Ok(())
}

fn shorten(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn fetch_expected_sha256(agent: &ureq::Agent, url: &str, image_name: &str) -> Result<String> {
    let body = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()
        .context("read SHA256SUMS")?;
    for line in body.lines() {
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*');
        if name == image_name {
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("malformed digest for {image_name} in SHA256SUMS");
            }
            return Ok(digest.to_ascii_lowercase());
        }
    }
    bail!("{image_name} not listed in {url}")
}

/// Stream `url` to `dest`, hashing as it goes. Returns the hex digest.
fn download_file(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    progress: &mut dyn ProgressSink,
) -> Result<String> {
    let response = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    let total: u64 = response
        .header("Content-Length")
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let mut reader = response.into_reader();
    let mut file = File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    loop {
        let count = reader.read(&mut buffer).context("read response body")?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])
            .with_context(|| format!("write {}", dest.display()))?;
        hasher.update(&buffer[..count]);
        done += count as u64;
        progress.progress(done, total);
        check_cancelled(progress)?;
    }
    file.sync_all().context("sync download")?;
    if total > 0 && done != total {
        bail!("download truncated: {done} of {total} bytes");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Archive member path as a path relative to the rootfs root, or `None`
/// for entries that must not be extracted (`..`, empty).
fn sanitize(path: &Path) -> Option<PathBuf> {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    if clean.as_os_str().is_empty() {
        None
    } else {
        Some(clean)
    }
}

fn remove_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
        }
        Ok(_) => fs::remove_file(path).with_context(|| format!("remove {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("stat {}", path.display())),
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    Ok(())
}

fn extract(
    tarball: &Path,
    staging: &Path,
    translator: &Translator<'_>,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    sweep(staging);
    fs::create_dir_all(staging).with_context(|| format!("create {}", staging.display()))?;
    let file = File::open(tarball).with_context(|| format!("open {}", tarball.display()))?;
    let mut archive = Archive::new(GzDecoder::new(file));

    let mut directory_modes: Vec<(PathBuf, u32)> = Vec::new();
    let mut count: u64 = 0;
    let mut skipped_special: u64 = 0;
    let mut rewritten_links: u64 = 0;
    for entry in archive.entries().context("read archive")? {
        let mut entry = entry.context("read archive entry")?;
        count += 1;
        if count.is_multiple_of(200) {
            progress.progress(count, 0);
            check_cancelled(progress)?;
        }
        let header = entry.header();
        let entry_type = header.entry_type();
        let mode = header.mode().unwrap_or(0o644) & 0o7777;
        let raw_path = entry.path().context("entry path")?.into_owned();
        let Some(relative) = sanitize(&raw_path) else {
            continue;
        };
        let dest = staging.join(&relative);
        match entry_type {
            EntryType::Directory => {
                fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
                directory_modes.push((dest, mode));
            }
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                ensure_parent(&dest)?;
                remove_existing(&dest)?;
                let mut out =
                    File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
                std::io::copy(&mut entry, &mut out)
                    .with_context(|| format!("write {}", dest.display()))?;
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode | 0o600))
                    .with_context(|| format!("chmod {}", dest.display()))?;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .context("symlink target")?
                    .context("symlink without target")?
                    .into_owned();
                let mut link_guest = Vec::with_capacity(relative.as_os_str().len() + 1);
                link_guest.push(b'/');
                link_guest.extend_from_slice(relative.as_os_str().as_bytes());
                let mut rewritten = [0u8; MAX_PATH];
                let mut scratch = [[0u8; MAX_PATH]; 2];
                let len = translator
                    .rewrite_symlink_target(&link_guest, target.as_os_str().as_bytes(), &mut rewritten, &mut scratch)
                    .map_err(|error| {
                        anyhow::anyhow!("symlink {} -> {}: {error:?}", relative.display(), target.display())
                    })?;
                let new_target = std::ffi::OsStr::from_bytes(&rewritten[..len]);
                if new_target != target.as_os_str() {
                    rewritten_links += 1;
                }
                ensure_parent(&dest)?;
                remove_existing(&dest)?;
                symlink(new_target, &dest).with_context(|| {
                    format!("symlink {} -> {}", dest.display(), new_target.to_string_lossy())
                })?;
            }
            EntryType::Link => {
                let target = entry
                    .link_name()
                    .context("hardlink target")?
                    .context("hardlink without target")?
                    .into_owned();
                let Some(target_relative) = sanitize(&target) else {
                    continue;
                };
                let source = staging.join(target_relative);
                ensure_parent(&dest)?;
                remove_existing(&dest)?;
                if let Err(error) = fs::hard_link(&source, &dest) {
                    log::debug!(
                        "rootfs_install: hard_link {} failed ({error}); copying",
                        dest.display()
                    );
                    fs::copy(&source, &dest).with_context(|| {
                        format!("copy {} -> {}", source.display(), dest.display())
                    })?;
                }
            }
            EntryType::Char | EntryType::Block | EntryType::Fifo => skipped_special += 1,
            _ => {}
        }
    }
    for (dir, mode) in directory_modes {
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode | 0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    progress.progress(count, count);
    log::info!(
        "rootfs_install: extracted {count} entries into {} ({rewritten_links} symlinks rewritten, {skipped_special} device nodes/fifos skipped)",
        staging.display()
    );
    Ok(())
}

/// Files the guest needs that the base image does not ship usable:
/// resolvers (Android has no resolv.conf), a hostname, and debugger
/// configs that keep SIGSYS traps from stopping gdb/lldb sessions.
fn seed(root: &Path, nameservers: &[String], bind_guest_dirs: &[String]) -> Result<()> {
    let write = |relative: &str, contents: String| -> Result<()> {
        let path = root.join(relative);
        ensure_parent(&path)?;
        remove_existing(&path)?;
        fs::write(&path, contents).with_context(|| format!("write {}", path.display()))
    };
    let resolv = nameservers
        .iter()
        .map(|server| format!("nameserver {server}\n"))
        .collect::<String>();
    write("etc/resolv.conf", resolv)?;
    write("etc/hostname", "zdroid\n".to_string())?;
    write(
        "etc/hosts",
        "127.0.0.1\tlocalhost\n127.0.1.1\tzdroid\n::1\tlocalhost ip6-localhost ip6-loopback\n"
            .to_string(),
    )?;
    write("root/.gitconfig", "[safe]\n\tdirectory = *\n".to_string())?;
    write(
        "etc/gdb/gdbinit",
        "handle SIGSYS nostop noprint pass\n".to_string(),
    )?;
    write(
        "root/.lldbinit",
        "process handle SIGSYS -n false -p true -s false\n".to_string(),
    )?;
    for dir in ["root/.cargo/bin", "root/.local/bin", "usr/local/bin", "tmp"] {
        let path = root.join(dir);
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    }
    // A binding's ancestors have to exist in the guest view, or realpath()
    // on a project path fails at `/data` (cargo and rustup canonicalize).
    for guest in bind_guest_dirs {
        let relative = guest.trim_start_matches('/');
        if relative.is_empty() {
            continue;
        }
        let path = root.join(relative);
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    }
    fs::set_permissions(root.join("tmp"), fs::Permissions::from_mode(0o1777))
        .context("chmod tmp")?;
    Ok(())
}

/// Refresh the resolver file on an installed rootfs (DNS servers change
/// with the network; boot calls this).
pub fn reseed_resolv(adapter: &RootfsAdapter, nameservers: &[String]) -> Result<()> {
    if !adapter.is_installed() {
        return Ok(());
    }
    let path = adapter.rootfs_dir().join("etc/resolv.conf");
    let resolv = nameservers
        .iter()
        .map(|server| format!("nameserver {server}\n"))
        .collect::<String>();
    remove_existing(&path)?;
    fs::write(&path, resolv).with_context(|| format!("write {}", path.display()))
}
