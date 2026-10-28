//! The Android code that Zed's crates call from small `cfg(target_os =
//! "android")` blocks: process spawning (`gpui_util`, `util::command`), the
//! local terminal's environment (`terminal`), npm's environment
//! (`node_runtime`) and a path lookup fallback (`project`). Keeping it here
//! leaves one call in each of those crates, which are merged at every Zed
//! sync.
//!
//! The app registers the active runtime's paths and variables at startup.
//! Switching runtimes restarts the app, so each is set once; a second
//! registration, when the Activity is recreated, is ignored.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::hash::BuildHasher;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Invoked by short name so the kernel's PATH lookup finds the
/// `$PREFIX/zd-runtime/zd-exec` symlink, like every other PATH-resolved
/// spawn, and this crate needs no app-package paths.
const ZD_EXEC_PROGRAM: &str = "zd-exec";

static ENVIRONMENT_ROOT: OnceLock<PathBuf> = OnceLock::new();
static TERMINAL_ENV: OnceLock<Vec<(String, OsString)>> = OnceLock::new();
static NPM_LIBTERMUX_EXEC_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

fn register<T>(slot: &OnceLock<T>, value: T, name: &str) {
    if slot.set(value).is_err() {
        log::debug!("zdroid_hooks: {name} already registered; ignoring the new value");
    }
}

/// Registers the host-side root of a runtime whose binaries can't run on the
/// host (the chroot's glibc userland). [`new_std_command`] then routes every
/// absolute program path under it through `zd-exec`.
pub fn register_environment_root(root: PathBuf) {
    register(&ENVIRONMENT_ROOT, root, "environment root");
}

/// Registers the variables the active runtime sets in local terminals, on
/// top of the app's own environment, which the shell inherits.
pub fn register_terminal_env(variables: Vec<(String, OsString)>) {
    register(&TERMINAL_ENV, variables, "terminal environment");
}

/// Registers Bootstrap's `libtermux-exec.so`, which rewrites the
/// `/data/data/com.termux/` paths built into Termux's npm packages. Other
/// runtimes register `None`.
pub fn register_npm_libtermux_exec_path(path: Option<PathBuf>) {
    register(&NPM_LIBTERMUX_EXEC_PATH, path, "npm libtermux-exec path");
}

/// `gpui_util::new_std_command` on Android, which `util::command::Command`
/// also starts from, so synchronous and asynchronous spawns are routed the
/// same way.
///
/// The app runs on bionic, but programs under the chroot's root need its
/// glibc loader (`/lib/ld-linux-aarch64.so.1`), which the host doesn't
/// have: those spawns become `zd-exec <program>`, which runs them inside
/// the runtime. A script starting with `#!/usr/bin/env <interpreter>` fails
/// the same way, because the host has no `/usr/bin/env`: it becomes
/// `<interpreter> <script>`, and the interpreter's short name resolves
/// through the runtime's `zd-runtime/<name>` symlinks. Anything else,
/// including system binaries such as `/system/bin/sh`, spawns unchanged.
pub fn new_std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let program = program.as_ref();
    if let Some(program_path) = environment_root_program(program) {
        let mut command = std::process::Command::new(ZD_EXEC_PROGRAM);
        command.arg(program_path);
        return command;
    }
    match env_shebang_interpreter(program) {
        Some((interpreter, script)) => {
            let mut command = std::process::Command::new(interpreter);
            command.arg(script);
            command
        }
        None => std::process::Command::new(program),
    }
}

fn environment_root_program(program: &OsStr) -> Option<PathBuf> {
    let root = ENVIRONMENT_ROOT.get()?;
    let path = Path::new(program);
    // `starts_with` compares whole components, so `<root>foo` doesn't match.
    (path.is_absolute() && path.starts_with(root)).then(|| path.to_path_buf())
}

/// Returns the interpreter and script path for an absolute `program` whose
/// first line is `#!/usr/bin/env <interpreter>`. Shebangs passing options
/// to env (`-S`), other shebangs, binaries and unreadable files return
/// `None` and spawn unchanged, so their original error surfaces.
fn env_shebang_interpreter(program: &OsStr) -> Option<(OsString, PathBuf)> {
    use std::io::{BufRead, BufReader, Read};

    let path = Path::new(program);
    if !path.is_absolute() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    // A shebang is short; don't read a large binary looking for a newline.
    let mut reader = BufReader::new(file.take(256));
    let mut first_line = String::new();
    reader.read_line(&mut first_line).ok()?;
    let interpreter = first_line
        .trim_end()
        .strip_prefix("#!/usr/bin/env ")?
        .split_whitespace()
        .next()?;
    if interpreter.starts_with('-') {
        return None;
    }
    Some((OsString::from(interpreter), path.to_path_buf()))
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
    if let Some(home) = std::env::var_os("TERMUX__HOME").or_else(|| std::env::var_os("HOME")) {
        let home = PathBuf::from(home);
        if home.is_dir() {
            log::debug!("zdroid_hooks: running in {home:?} instead of shared storage {dir:?}");
            return home;
        }
    }
    PathBuf::from("/system/bin")
}

/// Adds the active runtime's terminal variables to a local terminal's
/// environment. The shell inherits the app's environment, and these replace
/// what they name in it. Kept out of `terminal::insert_zed_terminal_env`,
/// which also builds the environment sent to SSH remotes, where these paths
/// don't exist.
pub fn insert_terminal_env<S: BuildHasher>(env: &mut HashMap<String, String, S>) {
    for (name, value) in TERMINAL_ENV.get().into_iter().flatten() {
        env.insert(name.clone(), value.to_string_lossy().into_owned());
    }
}

/// Adds what npm needs under Bootstrap to `node_runtime::npm_command_env`'s
/// environment: `libtermux-exec.so` to rewrite the Termux paths in package
/// scripts' shebangs, and the musl variants of packages with native
/// binaries, which run on Bootstrap's bundled musl loader. The chroot uses
/// its own libc and shebang paths, so it gets neither.
pub fn insert_npm_env<S: BuildHasher>(env: &mut HashMap<String, String, S>) {
    let Some(termux_exec) = NPM_LIBTERMUX_EXEC_PATH.get().and_then(Option::as_ref) else {
        return;
    };
    if termux_exec.is_file() {
        env.insert(
            "LD_PRELOAD".into(),
            termux_exec.to_string_lossy().into_owned(),
        );
    }
    env.insert("npm_config_libc".into(), "musl".into());
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
