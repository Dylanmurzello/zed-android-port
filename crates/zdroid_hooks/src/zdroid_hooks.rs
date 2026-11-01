//! The Android code that Zed's crates call from small `cfg(target_os =
//! "android")` blocks: process spawning (`gpui_util`, `util::command`), path
//! spelling (`fs`) and a path lookup fallback (`project`). Keeping it here
//! leaves one call in each of those crates, which are merged at every Zed
//! sync.
//!
//! The app registers the runtime's paths at startup. A second registration,
//! when the Activity is recreated, is ignored.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Invoked by short name: the kernel's PATH lookup finds the
/// `<files>/bin/zdload` link to the engine loader, so this crate needs no
/// app-package paths.
const BRIDGE_PROGRAM: &str = zd_engine_core::LOADER_LINK_NAME;

static ENVIRONMENT_ROOT: OnceLock<PathBuf> = OnceLock::new();
static FILES_DIR_SPELLINGS: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();

fn register<T>(slot: &OnceLock<T>, value: T, name: &str) {
    if slot.set(value).is_err() {
        log::debug!("zdroid_hooks: {name} already registered; ignoring the new value");
    }
}

/// Registers the app files dir. [`new_std_command`] then routes every
/// absolute program path under it through the engine loader.
pub fn register_environment_root(root: PathBuf) {
    register(&ENVIRONMENT_ROOT, root, "environment root");
}

/// Registers the files dir as the editor spells it (`/data/user/0/...`,
/// what its stored paths use) and as the kernel reports it
/// (`/data/data/...` at targetSdk 35). See `android-files-dir-spelling.md`.
pub fn register_files_dir_spellings(editor: PathBuf, kernel: PathBuf) {
    let spellings = (editor != kernel).then_some((editor, kernel));
    register(&FILES_DIR_SPELLINGS, spellings, "files dir spellings");
}

/// `path` with the kernel's spelling of the files dir replaced by the
/// editor's, or `None` when `path` isn't under it or the spellings agree.
pub fn respell_kernel_path(path: &Path) -> Option<PathBuf> {
    let (editor, kernel) = FILES_DIR_SPELLINGS.get()?.as_ref()?;
    let rest = path.strip_prefix(kernel).ok()?;
    Some(editor.join(rest))
}

/// `gpui_util::new_std_command` on Android, which `util::command::Command`
/// also starts from, so synchronous and asynchronous spawns are routed the
/// same way.
///
/// The app runs on bionic, but the programs Zed keeps under the files dir
/// (its Node, downloaded language servers and debug adapters) are Linux
/// binaries for the runtime's Ubuntu userland: those spawns become `zdload
/// <program>`, which maps the program into the rootfs. The loader handles
/// `#!` lines itself. Anything else, including system binaries such as
/// `/system/bin/sh` and programs found on PATH, spawns unchanged.
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let program = program.as_ref();
    match environment_root_program(program) {
        Some(program_path) => {
            let mut command = std::process::Command::new(BRIDGE_PROGRAM);
            command.arg(program_path);
            command
        }
        None => std::process::Command::new(program),
    }
}

fn environment_root_program(program: &OsStr) -> Option<PathBuf> {
    let root = ENVIRONMENT_ROOT.get()?;
    let path = Path::new(program);
    if !path.is_absolute() {
        return None;
    }
    // A path a tool or the kernel produced may use the kernel's spelling.
    let respelled = respell_kernel_path(path);
    let path = respelled.as_deref().unwrap_or(path);
    // `starts_with` compares whole components, so `<root>foo` doesn't match.
    if !path.starts_with(root) {
        return None;
    }
    // `zd-runtime/` and `bin/` hold links to the loader itself, which would
    // open its own link as the guest program; the kernel execs them.
    if path.starts_with(root.join("zd-runtime")) || path.starts_with(root.join("bin")) {
        return None;
    }
    Some(path.to_path_buf())
}

/// `util::command::Command::env` on Android, for `PATH`.
///
/// The loader replaces a child's environment with the guest view in
/// `ZD_GUEST_ENV` at every exec the editor starts, so a `PATH` set on one
/// spawn would never reach the guest. npm runs package scripts as `sh -c
/// "node install.js"` with the managed Node's `bin` on that `PATH`. Returns
/// the `ZD_GUEST_ENV` to set on the child, with `host_path`'s entries ahead
/// of the guest's own `PATH`, or `None` when the app doesn't run the engine.
pub fn guest_env_for_path(host_path: &OsStr) -> Option<(&'static str, OsString)> {
    let spec = std::env::var(zd_engine_core::env::GUEST_ENV).ok()?;
    let root = ENVIRONMENT_ROOT.get().map(PathBuf::as_path);
    let merged = merge_guest_path(&spec, &host_path.to_string_lossy(), root);
    Some((zd_engine_core::env::GUEST_ENV, merged.into()))
}

/// Puts `host_path`'s entries ahead of the `PATH` in the guest environment
/// `spec`. Android's partitions and the loader links in `zd-runtime/` and
/// `bin/` don't exist in the guest, so they're dropped.
fn merge_guest_path(spec: &str, host_path: &str, root: Option<&Path>) -> String {
    const HOST_ONLY_PREFIXES: &[&str] = &[
        "/system/",
        "/apex/",
        "/vendor/",
        "/product/",
        "/odm/",
        "/data/app/",
    ];
    let host_only = |entry: &str| {
        HOST_ONLY_PREFIXES
            .iter()
            .any(|prefix| entry.starts_with(prefix))
            || root.is_some_and(|root| {
                let path = Path::new(entry);
                path.starts_with(root.join("zd-runtime")) || path.starts_with(root.join("bin"))
            })
    };
    let mut entries: Vec<&str> = Vec::new();
    for entry in host_path.split(':') {
        if entry.starts_with('/') && !host_only(entry) && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    let separator = zd_engine_core::LIST_SEPARATOR.to_string();
    let mut items: Vec<String> = Vec::new();
    let mut replaced = false;
    for item in spec.split(&separator) {
        if let Some(guest_path) = item.strip_prefix("PATH=") {
            for entry in guest_path.split(':') {
                if !entry.is_empty() && !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
            items.push(format!("PATH={}", entries.join(":")));
            replaced = true;
        } else if !item.is_empty() {
            items.push(item.to_string());
        }
    }
    if !replaced {
        items.push(format!("PATH={}", entries.join(":")));
    }
    items.join(&separator)
}

/// `util::command::Command::current_dir` on Android.
///
/// A spawned process's `chdir(2)` into Android's FUSE-emulated shared
/// storage fails with ENOENT, even though the app itself can list and open
/// files there, so every language server or build tool spawned for a
/// project on `/sdcard` failed. Such directories are replaced with the
/// runtime's home. Language servers still get the project's path through
/// `workspaceFolders`.
pub fn safe_current_dir(dir: &Path) -> PathBuf {
    const SHARED_STORAGE_PREFIXES: &[&str] = &[
        "/storage/emulated/",
        "/storage/self/",
        "/sdcard/",
        "/mnt/runtime/",
        "/mnt/user/",
        "/mnt/media_rw/",
    ];
    let dir_string = dir.to_string_lossy();
    if !SHARED_STORAGE_PREFIXES
        .iter()
        .any(|prefix| dir_string.starts_with(prefix))
    {
        return dir.to_path_buf();
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if home.is_dir() {
            log::debug!("zdroid_hooks: running in {home:?} instead of shared storage {dir:?}");
            return home;
        }
    }
    PathBuf::from("/system/bin")
}

/// `child`'s path relative to `parent`, when one of `child`'s ancestors is
/// the same directory as `parent` under another name. Android reaches the
/// app's data directory as both `/data/data/<package>` and
/// `/data/user/0/<package>`: two bind mounts of one directory, which
/// `canonicalize` doesn't unify, so the device and inode numbers are
/// compared instead.
pub fn strip_prefix_by_identity(child: &Path, parent: &Path) -> Option<PathBuf> {
    let parent_identity = file_identity(parent)?;
    child
        .ancestors()
        .find(|ancestor| file_identity(ancestor) == Some(parent_identity))
        .and_then(|ancestor| child.strip_prefix(ancestor).ok())
        .map(Path::to_path_buf)
}

fn file_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "HOME=/root;PATH=/usr/local/bin:/usr/bin:/bin;TMPDIR=/tmp";

    #[test]
    fn managed_node_dir_leads_and_host_dirs_drop() {
        let root = Path::new("/data/user/0/com.zdroid/files");
        let host_path = "/data/user/0/com.zdroid/files/node/v24/bin:/data/user/0/com.zdroid/files/zd-runtime:/data/user/0/com.zdroid/files/bin:/system/bin";
        assert_eq!(
            merge_guest_path(SPEC, host_path, Some(root)),
            "HOME=/root;PATH=/data/user/0/com.zdroid/files/node/v24/bin:/usr/local/bin:/usr/bin:/bin;TMPDIR=/tmp"
        );
    }

    #[test]
    fn a_captured_guest_path_keeps_its_order() {
        let host_path = "/root/.local/bin:/usr/bin:/usr/local/bin:/bin";
        assert_eq!(
            merge_guest_path(SPEC, host_path, None),
            "HOME=/root;PATH=/root/.local/bin:/usr/bin:/usr/local/bin:/bin;TMPDIR=/tmp"
        );
    }

    #[test]
    fn a_spec_without_path_gains_one() {
        assert_eq!(
            merge_guest_path("HOME=/root", "/opt/bin:relative:/system/bin", None),
            "HOME=/root;PATH=/opt/bin"
        );
    }

    #[test]
    fn strip_prefix_by_identity_finds_an_ancestor() {
        let directory = std::env::temp_dir();
        let child = directory.join("nested").join("file.txt");
        assert_eq!(
            strip_prefix_by_identity(&child, &directory),
            Some(PathBuf::from("nested/file.txt"))
        );
        assert_eq!(
            strip_prefix_by_identity(&directory, &directory),
            Some(PathBuf::new())
        );
    }

    #[test]
    fn strip_prefix_by_identity_without_a_match() {
        let parent = std::env::temp_dir();
        assert_eq!(
            strip_prefix_by_identity(Path::new("/no/such/path"), &parent),
            None
        );
        assert_eq!(
            strip_prefix_by_identity(&parent, Path::new("/no/such/path")),
            None
        );
    }
}
