//! Extract the bundled Ubuntu base image and activate it.
//!
//! The extractor is deliberately not `tar::Archive::unpack`: every
//! symlink is rewritten so the kernel can resolve it from the host side
//! (the engine's relative-symlink invariant), device nodes and fifos are
//! skipped because `/dev` is bound from the host, ownership is ignored
//! (everything belongs to the app uid; the loader presents uid 0), and
//! modes are kept so setuid bits and executable bits survive. The new
//! tree is built next to the live one and swapped in with two renames.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use tar::{Archive, EntryType};
use zd_engine_core::bindings::Bindings;
use zd_engine_core::path::MAX_PATH;
use zd_engine_core::translate::Translator;

use crate::progress::ProgressSink;
use crate::runtime::{Runtime, VERSION_FILE};

/// The image the APK bundles, as named in `rootfs-image.properties`
/// (one file read by gradle and compiled in here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub name: String,
    pub sha256: String,
    pub url: String,
}

/// Path of the bundled image inside the APK's assets. A fixed name without
/// a `.gz` suffix: aapt gunzips `.gz` assets at packaging time.
pub const IMAGE_ASSET_PATH: &str = "rootfs/image.tgz";

pub fn image() -> &'static Image {
    static IMAGE: LazyLock<Image> = LazyLock::new(|| {
        let mut image = Image {
            name: String::new(),
            sha256: String::new(),
            url: String::new(),
        };
        for line in include_str!("../rootfs-image.properties").lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "name" => image.name = value.trim().to_string(),
                "sha256" => image.sha256 = value.trim().to_string(),
                "url" => image.url = value.trim().to_string(),
                _ => {}
            }
        }
        image
    });
    &IMAGE
}

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
/// The SSH client is what Zed runs for remote projects and git's SSH
/// remotes, and git only recommends it.
const ESSENTIALS_SCRIPT: &str = "apt-get update && apt-get install -y --no-install-recommends \
     ca-certificates git openssh-client";

fn sibling(rootfs: &Path, suffix: &str) -> PathBuf {
    let mut name = rootfs
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "rootfs".into());
    name.push(".");
    name.push(suffix);
    rootfs.with_file_name(name)
}

/// Extract `image` (the gzip tarball) into a fresh rootfs and make it the
/// live one. An existing rootfs is replaced wholesale: this is both the
/// first-launch setup and the reset.
pub fn install(
    runtime: &Runtime,
    image: &mut dyn Read,
    nameservers: &[String],
    timezone: &str,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    let rootfs = runtime.rootfs_dir();
    let staging = sibling(&rootfs, "staging");
    let old = sibling(&rootfs, "old");

    preflight(runtime, &[&staging, &old])?;
    let result = install_inner(
        runtime,
        image,
        nameservers,
        timezone,
        progress,
        &staging,
        &old,
    );
    if result.is_err() {
        sweep(&staging);
    }
    result
}

fn preflight(runtime: &Runtime, stale: &[&Path]) -> Result<()> {
    let loader = runtime.loader_path();
    if !loader.exists() {
        bail!(
            "engine loader missing at {} (reinstall the app)",
            loader.display()
        );
    }
    let rootfs = runtime.rootfs_dir();
    let parent = rootfs.parent().context("rootfs dir has no parent")?;
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
        Ok(()) => log::info!("install: removed {}", path.display()),
        Err(error) => log::warn!("install: could not remove {}: {error}", path.display()),
    }
}

fn check_cancelled(progress: &dyn ProgressSink) -> Result<()> {
    if progress.is_cancelled() {
        return Err(Cancelled.into());
    }
    Ok(())
}

fn install_inner(
    runtime: &Runtime,
    image: &mut dyn Read,
    nameservers: &[String],
    timezone: &str,
    progress: &mut dyn ProgressSink,
    staging: &Path,
    old: &Path,
) -> Result<()> {
    progress.step("Extracting");
    let translator_rootfs = runtime.kernel_rootfs_dir().as_os_str().as_bytes().to_vec();
    let bind_spec = runtime.bind_spec().into_bytes();
    let mut bindings = Bindings::with_defaults();
    bindings
        .parse_spec(&bind_spec)
        .map_err(|error| anyhow::anyhow!("bind spec: {error:?}"))?;
    let translator = Translator::new(&translator_rootfs, bindings);
    extract(image, staging, &translator, progress)?;

    progress.step("Seeding configuration");
    seed(staging, nameservers, timezone, &runtime.bind_guest_dirs())?;
    let image = self::image();
    fs::write(
        staging.join(VERSION_FILE),
        format!("{}\n{}\n", image.name, image.sha256),
    )
    .context("write version file")?;

    progress.step("Activating");
    let rootfs = runtime.rootfs_dir();
    if rootfs.exists() {
        fs::rename(&rootfs, old)
            .with_context(|| format!("rename {} -> {}", rootfs.display(), old.display()))?;
    }
    fs::rename(staging, &rootfs)
        .with_context(|| format!("rename {} -> {}", staging.display(), rootfs.display()))?;
    // The guest's home outlives a reset: dotfiles, SSH keys, logins and
    // rustup's toolchains stay. On an error the old tree, home included,
    // is left in place.
    let old_home = old.join("root");
    if old_home.is_dir() {
        let home = rootfs.join("root");
        remove_existing(&home)?;
        fs::rename(&old_home, &home)
            .with_context(|| format!("rename {} -> {}", old_home.display(), home.display()))?;
    }
    sweep(old);
    if let Err(error) = link_home(runtime) {
        progress.warn(&format!("linking the home folder failed: {error:#}"));
    }
    log::info!(
        "install: {} ({}) active at {}",
        image.name,
        image.sha256,
        rootfs.display()
    );
    Ok(())
}

/// Things the guest's home (`/root`) reaches in the app's home folder
/// (`<data>/files/home`), which Zed's project picker and the `~/storage`
/// links use, and which held the home of the Bootstrap runtime that
/// Zdroid had before this one.
const LINKED_FROM_APP_HOME: &[&str] = &["projects", "storage"];
const LINKED_IF_PRESENT: &[&str] = &[".ssh", ".claude", ".claude.json", ".codex"];

/// Links `/root/<name>` to the app's home folder for [`LINKED_FROM_APP_HOME`]
/// and for whichever of [`LINKED_IF_PRESENT`] a Bootstrap-era home holds,
/// and makes the guest's git config include that home's. A name the guest's
/// home already has is left alone. The targets are identity-mapped, so a
/// tool's `getcwd` under `~/projects` reports the path Zed uses; binding
/// `/root` to the folder instead would make the engine's reverse lookup
/// report `/root/projects/...`.
fn link_home(runtime: &Runtime) -> Result<()> {
    let home = runtime.rootfs_dir().join("root");
    let app_home = runtime.workspace_root();
    let present = LINKED_IF_PRESENT
        .iter()
        .filter(|name| fs::symlink_metadata(app_home.join(name)).is_ok());
    for name in LINKED_FROM_APP_HOME.iter().chain(present) {
        let link = home.join(name);
        if fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        symlink(app_home.join(name), &link).with_context(|| format!("link {}", link.display()))?;
    }
    let app_gitconfig = app_home.join(".gitconfig");
    if app_gitconfig.is_file() {
        let gitconfig = home.join(".gitconfig");
        let include = format!("[include]\n\tpath = {}\n", app_gitconfig.display());
        let current = fs::read_to_string(&gitconfig).unwrap_or_default();
        if !current.contains(&include) {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&gitconfig)
                .with_context(|| format!("open {}", gitconfig.display()))?;
            std::io::Write::write_all(&mut file, include.as_bytes())
                .with_context(|| format!("write {}", gitconfig.display()))?;
        }
    }
    Ok(())
}

/// `ubuntu-base` ships neither git nor CA certificates, so this runs
/// apt through the loader. Network-bound and slow, so it is its own
/// step after the rootfs is live; the rootfs is complete without it.
// Synchronous on purpose: callers run it on their own thread and the
// runtime spawns the loader directly.
#[allow(clippy::disallowed_methods)]
pub fn install_essentials(runtime: &Runtime, progress: &mut dyn ProgressSink) -> Result<()> {
    progress.step("Installing git, the SSH client and certificates");
    let mut command = runtime.command("/bin/sh");
    command
        .args(["-c", ESSENTIALS_SCRIPT])
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("spawn apt-get through the loader")?;
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
            log::warn!("install: reading apt-get stderr: {error}");
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
        return Err(Cancelled.into());
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

/// Whether the essentials step has something left to do.
pub fn essentials_missing(runtime: &Runtime) -> bool {
    let rootfs = runtime.rootfs_dir();
    !rootfs.join("usr/bin/git").exists()
        || !rootfs.join("etc/ssl/certs/ca-certificates.crt").exists()
}

fn shorten(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
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
    image: &mut dyn Read,
    staging: &Path,
    translator: &Translator<'_>,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    sweep(staging);
    fs::create_dir_all(staging).with_context(|| format!("create {}", staging.display()))?;
    let mut archive = Archive::new(GzDecoder::new(image));

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
                    .rewrite_symlink_target(
                        &link_guest,
                        target.as_os_str().as_bytes(),
                        &mut rewritten,
                        &mut scratch,
                    )
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "symlink {} -> {}: {error:?}",
                            relative.display(),
                            target.display()
                        )
                    })?;
                let new_target = std::ffi::OsStr::from_bytes(&rewritten[..len]);
                if new_target != target.as_os_str() {
                    rewritten_links += 1;
                }
                ensure_parent(&dest)?;
                remove_existing(&dest)?;
                symlink(new_target, &dest).with_context(|| {
                    format!(
                        "symlink {} -> {}",
                        dest.display(),
                        new_target.to_string_lossy()
                    )
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
                        "install: hard_link {} failed ({error}); copying",
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
        "install: extracted {count} entries into {} ({rewritten_links} symlinks rewritten, {skipped_special} device nodes/fifos skipped)",
        staging.display()
    );
    Ok(())
}

/// Files the guest needs that the base image does not ship usable:
/// resolvers (Android has no resolv.conf), a hostname, and debugger
/// configs that keep SIGSYS traps from stopping gdb/lldb sessions.
fn seed(
    root: &Path,
    nameservers: &[String],
    timezone: &str,
    bind_guest_dirs: &[String],
) -> Result<()> {
    let write = |relative: &str, contents: String| -> Result<()> {
        let path = root.join(relative);
        ensure_parent(&path)?;
        remove_existing(&path)?;
        fs::write(&path, contents).with_context(|| format!("write {}", path.display()))
    };
    write("etc/resolv.conf", resolv_conf(nameservers))?;
    seed_timezone(root, timezone)?;
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

fn resolv_conf(nameservers: &[String]) -> String {
    nameservers
        .iter()
        .map(|server| format!("nameserver {server}\n"))
        .collect()
}

/// `/etc/localtime` and `/etc/timezone` for the device's zone. The base
/// image ships no tzdata, so the link dangles until the first `apt`
/// install pulls it in; that is also what keeps tzdata's debconf
/// questions quiet then (its config script takes the zone from an
/// existing link and marks the questions seen), where a bare rootfs
/// stops every first `apt install` at an interactive time zone menu.
/// The link is relative, the installer's invariant for the whole tree.
fn seed_timezone(root: &Path, timezone: &str) -> Result<()> {
    let zone = if crate::is_zone_id(timezone) {
        timezone
    } else {
        crate::FALLBACK_TIMEZONE
    };
    let localtime = root.join("etc/localtime");
    ensure_parent(&localtime)?;
    remove_existing(&localtime)?;
    std::os::unix::fs::symlink(format!("../usr/share/zoneinfo/{zone}"), &localtime)
        .with_context(|| format!("link {}", localtime.display()))?;
    let timezone_file = root.join("etc/timezone");
    remove_existing(&timezone_file)?;
    fs::write(&timezone_file, format!("{zone}\n"))
        .with_context(|| format!("write {}", timezone_file.display()))
}

/// Seed the time zone on an installed rootfs that has none (an install
/// from before the seed existed). A zone the user or tzdata chose stays.
pub fn reseed_timezone(runtime: &Runtime, timezone: &str) -> Result<()> {
    if !runtime.is_installed() {
        return Ok(());
    }
    let localtime = runtime.rootfs_dir().join("etc/localtime");
    if fs::symlink_metadata(&localtime).is_ok() {
        return Ok(());
    }
    seed_timezone(&runtime.rootfs_dir(), timezone)
}

/// Refresh the resolver file on an installed rootfs (DNS servers change
/// with the network; boot and Android's network callback call this). The
/// new file is renamed into place, so a lookup running meanwhile reads
/// either the old servers or the new ones, never no file.
pub fn reseed_resolv(runtime: &Runtime, nameservers: &[String]) -> Result<()> {
    if !runtime.is_installed() {
        return Ok(());
    }
    let path = runtime.rootfs_dir().join("etc/resolv.conf");
    let contents = resolv_conf(nameservers);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file())
        && fs::read_to_string(&path).is_ok_and(|current| current == contents)
    {
        return Ok(());
    }
    let staged = path.with_extension("conf.new");
    fs::write(&staged, contents).with_context(|| format!("write {}", staged.display()))?;
    fs::rename(&staged, &path)
        .with_context(|| format!("rename {} -> {}", staged.display(), path.display()))
}
