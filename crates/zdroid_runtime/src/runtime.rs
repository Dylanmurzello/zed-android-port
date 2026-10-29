//! The runtime handle: where the rootfs and the loader live, and the
//! environment the editor and its children run with.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use zd_engine_core::{DEFAULT_GUEST_PATH, LIST_SEPARATOR, LOADER_LINK_NAME, env as engine_env};

use crate::env::EnvOp;

/// Name of the per-install sentinel the installer writes last:
/// `<image name>\n<sha256>\n`.
pub const VERSION_FILE: &str = ".rootfs-version";

/// Dev toggle: when this file exists the loader appends diagnostics to
/// `<data>/files/zd-engine.log`.
pub const LOG_TOGGLE_FILE: &str = "etc/zd-engine-log";

/// The node name `uname` reports in the guest; the installer seeds the
/// same into `etc/hostname`.
pub const HOSTNAME: &str = "zdroid";

/// What the integrated terminal execs: a program plus the arguments
/// that turn it into the user's shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalShell {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// The image an installed rootfs was extracted from, as recorded in the
/// sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledImage {
    pub name: String,
    pub sha256: String,
}

pub struct Runtime {
    data_path: PathBuf,
    /// `data_path` as the kernel reports it. The kernel names a file by
    /// the route it walked, and on Android 16 at targetSdk 35 the app's
    /// files dir is reached through the `/data/data/<pkg>` bind mount
    /// (`/data/user/0` is a symlink there), so `getcwd` and `/proc/*/cwd`
    /// spell it `/data/data/...` while Zed spells it `/data/user/0/...`.
    /// The engine compares the kernel's spelling against the rootfs and
    /// the binding hosts, so those use this form; everything Zed opens
    /// itself keeps `data_path`.
    kernel_data_path: PathBuf,
}

impl Runtime {
    pub fn new(data_path: &Path) -> Self {
        let kernel_data_path =
            std::fs::canonicalize(data_path).unwrap_or_else(|_| data_path.to_path_buf());
        Self {
            data_path: data_path.to_path_buf(),
            kernel_data_path,
        }
    }

    /// The files dir in the kernel's spelling (see the field).
    pub fn kernel_data_path(&self) -> &Path {
        &self.kernel_data_path
    }

    /// The rootfs dir as handed to the engine (`ZD_ROOTFS`).
    pub fn kernel_rootfs_dir(&self) -> PathBuf {
        self.kernel_data_path.join("rootfs")
    }

    /// The app files dir: Zed's data root and the identity-mapped part of
    /// the guest view.
    pub fn data_path(&self) -> &Path {
        &self.data_path
    }

    pub fn rootfs_dir(&self) -> PathBuf {
        self.data_path.join("rootfs")
    }

    /// The stable link boot re-points at the current install's loader.
    pub fn loader_path(&self) -> PathBuf {
        self.data_path.join("bin").join(LOADER_LINK_NAME)
    }

    pub fn version_file(&self) -> PathBuf {
        self.rootfs_dir().join(VERSION_FILE)
    }

    pub fn is_installed(&self) -> bool {
        self.version_file().is_file()
    }

    /// The image the installed rootfs came from, `None` when nothing is
    /// installed or the sentinel is unreadable.
    pub fn installed_image(&self) -> Option<InstalledImage> {
        let sentinel = std::fs::read_to_string(self.version_file()).ok()?;
        let mut lines = sentinel.lines();
        let name = lines.next()?.trim();
        let sha256 = lines.next()?.trim();
        if name.is_empty() || sha256.is_empty() {
            return None;
        }
        Some(InstalledImage {
            name: name.to_string(),
            sha256: sha256.to_string(),
        })
    }

    /// Where the user's projects live; the recent-projects UI groups
    /// paths under it as "Workspace".
    pub fn workspace_root(&self) -> PathBuf {
        self.data_path.join("home")
    }

    /// Link target, relative to `<data>/files/zd-runtime/`, for every entry
    /// of the symlink farm.
    pub fn farm_link_target(&self) -> PathBuf {
        PathBuf::from("../bin").join(LOADER_LINK_NAME)
    }

    /// Guest-to-host bindings handed to the loader. Both spellings of
    /// the app files dir are identity-mapped so project paths mean the
    /// same thing on either side of the boundary.
    pub fn bind_spec(&self) -> String {
        let host = self.kernel_data_path.to_string_lossy();
        let zed_spelling = self.data_path.to_string_lossy();
        // Guest spellings of the files dir, Zed's first: for equal host
        // prefixes the engine maps a kernel path back to the first entry,
        // so `getcwd` under a project reports the spelling Zed passed.
        let mut binds = vec![
            format!("/tmp:{host}/tmp"),
            format!("/dev/shm:{host}/tmp/shm"),
            format!("/data/user/0/com.zdroid/files:{host}"),
            format!("/data/data/com.zdroid/files:{host}"),
        ];
        if !binds
            .iter()
            .any(|entry| entry.starts_with(&format!("{zed_spelling}:")))
        {
            binds.push(format!("{zed_spelling}:{host}"));
        }
        binds.push("/sdcard".to_string());
        binds.push("/storage".to_string());
        binds.join(&LIST_SEPARATOR.to_string())
    }

    /// The guest paths of every binding, for the installer's directory
    /// chains.
    pub fn bind_guest_dirs(&self) -> Vec<String> {
        self.bind_spec()
            .split(LIST_SEPARATOR)
            .map(|entry| {
                entry
                    .split_once(':')
                    .map_or(entry, |(guest, _host)| guest)
                    .to_string()
            })
            .collect()
    }

    /// Variables applied to the guest environment only.
    pub fn guest_env_spec(&self) -> String {
        [
            "HOME=/root",
            "USER=root",
            "LOGNAME=root",
            "SHELL=/bin/bash",
            &format!("PATH={DEFAULT_GUEST_PATH}"),
            "LANG=C.UTF-8",
            // The host's `<data>/tmp` is the guest's `/tmp` (bound), so the
            // guest spelling keeps temp paths short and native-looking.
            "TMPDIR=/tmp",
        ]
        .join(&LIST_SEPARATOR.to_string())
    }

    /// The `ZD_*` block every loader invocation needs.
    pub fn engine_env(&self) -> Vec<(String, OsString)> {
        let mut vars = vec![
            (
                engine_env::ROOTFS.to_string(),
                self.kernel_rootfs_dir().into_os_string(),
            ),
            (
                engine_env::BINDS.to_string(),
                OsString::from(self.bind_spec()),
            ),
            (engine_env::FAKE_ROOT.to_string(), OsString::from("1")),
            (engine_env::HOSTNAME.to_string(), OsString::from(HOSTNAME)),
            (engine_env::CLOSE_FDS.to_string(), OsString::from("1")),
            (
                engine_env::GUEST_ENV.to_string(),
                OsString::from(self.guest_env_spec()),
            ),
        ];
        if self.data_path.join(LOG_TOGGLE_FILE).exists() {
            vars.push((
                engine_env::LOG.to_string(),
                self.data_path.join("zd-engine.log").into_os_string(),
            ));
        }
        vars
    }

    /// Mutations to apply to the Zed process env at boot. Zed's own
    /// process keeps the host shape (HOME, PATH with the farm first); the
    /// engine block rides along so every child spawned from it, through
    /// the farm or the bridge, reaches the loader configured.
    pub fn env_for_zed_process(&self) -> Vec<(String, EnvOp)> {
        let mut ops = crate::env::baseline_env(&self.data_path);
        for (key, value) in self.engine_env() {
            ops.push((key, EnvOp::Set(value)));
        }
        ops.push((
            "SHELL".into(),
            EnvOp::Set(self.loader_path().into_os_string()),
        ));
        ops
    }

    /// The loader under its own name with a leading flag runs the
    /// default shell with that flag: `/bin/bash -l`.
    pub fn terminal_shell(&self) -> TerminalShell {
        TerminalShell {
            program: self.loader_path(),
            args: vec!["-l".to_string()],
        }
    }

    /// The host directories holding the guest's programs, the ones on the
    /// guest's PATH.
    pub fn binary_dirs(&self) -> Vec<PathBuf> {
        let rootfs = self.rootfs_dir();
        [
            "usr/bin",
            "usr/local/bin",
            "usr/sbin",
            "root/.cargo/bin",
            "root/.local/bin",
        ]
        .into_iter()
        .map(|dir| rootfs.join(dir))
        .collect()
    }

    /// Names of the programs the guest's standard directories hold; the
    /// symlink farm mirrors them so Zed's PATH lookup finds them. `bin`
    /// and `sbin` are links to their `usr/` directories in Ubuntu.
    pub fn list_binaries(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for dir in self.binary_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && !name.starts_with('.')
                {
                    names.insert(name.to_string());
                }
            }
        }
        names.into_iter().collect()
    }

    /// A command that runs `program` (a guest path) through the loader
    /// with the caller's environment plus the engine block. The loader is
    /// exec'd with the guest's own argv (`argv[0]` = `program`) and learns
    /// what to run from `ZD_PROGRAM`, so the kernel's argument area, and
    /// with it `/proc/<pid>/cmdline`, is exactly the guest's.
    pub fn command(&self, program: &str) -> Command {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(self.loader_path());
        command.arg0(program);
        command.env(engine_env::PROGRAM, program);
        command.envs(self.engine_env());
        command
    }

    /// Run a guest program to completion with a deadline, capturing its
    /// output. Used by the engine self-test.
    // The runtime is the loader's spawner itself and is only called from
    // background threads; the smol wrapper would route back through the
    // spawn bridge.
    #[allow(clippy::disallowed_methods)]
    pub fn run_captured(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        deadline: Duration,
    ) -> Result<CapturedRun> {
        let mut command = self.command(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let started = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {} {program}", self.loader_path().display()))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_reader = std::thread::spawn(move || read_all(stdout));
        let stderr_reader = std::thread::spawn(move || read_all(stderr));
        let status = loop {
            if let Some(status) = child.try_wait().context("wait for guest")? {
                break status;
            }
            if started.elapsed() > deadline {
                if let Err(error) = child.kill() {
                    log::warn!("runtime: kill after deadline failed: {error}");
                }
                let status = child.wait().context("wait after kill")?;
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader.join().unwrap_or_default();
                return Ok(CapturedRun {
                    code: status.code(),
                    timed_out: true,
                    elapsed: started.elapsed(),
                    stdout,
                    stderr,
                });
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Ok(CapturedRun {
            code: status.code(),
            timed_out: false,
            elapsed: started.elapsed(),
            stdout: stdout_reader.join().unwrap_or_default(),
            stderr: stderr_reader.join().unwrap_or_default(),
        })
    }
}

fn read_all<R: Read>(reader: Option<R>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut reader) = reader
        && let Err(error) = reader.read_to_end(&mut buffer)
    {
        log::warn!("runtime: reading guest output failed: {error}");
    }
    buffer
}

pub struct CapturedRun {
    /// Exit code, `None` when killed by a signal.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub elapsed: Duration,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CapturedRun {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::Runtime;

    #[test]
    fn engine_paths_use_the_kernel_spelling_and_zed_keeps_its_own() {
        let temp = tempfile::tempdir().expect("tempdir");
        let given = temp.path().to_path_buf();
        let canonical = std::fs::canonicalize(&given).expect("canonicalize");
        let runtime = Runtime::new(&given);
        assert_eq!(runtime.data_path(), given);
        assert_eq!(runtime.rootfs_dir(), given.join("rootfs"));
        assert_eq!(runtime.kernel_rootfs_dir(), canonical.join("rootfs"));
        let spec = runtime.bind_spec();
        let host = canonical.to_string_lossy();
        let entries: Vec<&str> = spec.split(';').collect();
        assert_eq!(entries[2], format!("/data/user/0/com.zdroid/files:{host}"));
        assert_eq!(entries[3], format!("/data/data/com.zdroid/files:{host}"));
        assert!(entries.contains(&format!("{}:{host}", given.to_string_lossy()).as_str()));
        assert!(!spec.contains(&format!(":{}/", given.to_string_lossy())) || given == canonical);
    }
}
