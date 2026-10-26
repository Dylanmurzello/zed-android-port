//! Rootfs adapter: a stock Ubuntu base image under `<data>/files/rootfs`,
//! every process exec'd through the engine loader (`libzdload.so`, reached
//! via the stable `<data>/files/bin/zdload` link).
//!
//! The loader maps the guest binary itself and translates path syscalls
//! into the rootfs, so nothing in the image is patched and the adapter's
//! job is only to install the image and shape the environment.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use zd_engine_core::{DEFAULT_GUEST_PATH, LIST_SEPARATOR, LOADER_LINK_NAME, env as engine_env};

use crate::config::{self, RootfsConfig, RuntimeId};
use crate::health::{HealthStatus, ProgressSink};
use crate::port::{RuntimeProvider, SpawnHandle, SpawnRequest, TerminalShell};

/// Name of the per-install sentinel the installer writes last.
pub const VERSION_FILE: &str = ".rootfs-version";

/// Dev toggle: when this file exists the loader appends diagnostics to
/// `<data>/files/zd-engine.log`.
pub const LOG_TOGGLE_FILE: &str = "etc/zd-engine-log";

pub struct RootfsAdapter {
    config: RootfsConfig,
    data_path: PathBuf,
}

impl RootfsAdapter {
    pub fn new(config: RootfsConfig, data_path: &Path) -> Self {
        Self {
            config,
            data_path: data_path.to_path_buf(),
        }
    }

    pub fn config(&self) -> &RootfsConfig {
        &self.config
    }

    pub fn rootfs_dir(&self) -> &Path {
        &self.config.rootfs_dir
    }

    /// The stable link boot re-points at the current install's loader.
    pub fn loader_path(&self) -> PathBuf {
        self.data_path.join("bin").join(LOADER_LINK_NAME)
    }

    pub fn version_file(&self) -> PathBuf {
        self.config.rootfs_dir.join(VERSION_FILE)
    }

    pub fn is_installed(&self) -> bool {
        self.version_file().is_file()
    }

    /// Guest-to-host bindings handed to the loader. Both spellings of
    /// the app files dir are identity-mapped so project paths mean the
    /// same thing on either side of the boundary.
    pub fn bind_spec(&self) -> String {
        let files = self.data_path.to_string_lossy();
        let mut binds = vec![
            format!("/tmp:{files}/tmp"),
            format!("/dev/shm:{files}/tmp/shm"),
            files.to_string(),
            format!("/data/data/com.zdroid/files:{files}"),
            format!("/data/user/0/com.zdroid/files:{files}"),
            "/sdcard".to_string(),
            "/storage".to_string(),
        ];
        binds.dedup();
        binds.join(&LIST_SEPARATOR.to_string())
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
                self.config.rootfs_dir.clone().into_os_string(),
            ),
            (engine_env::BINDS.to_string(), OsString::from(self.bind_spec())),
            (engine_env::FAKE_ROOT.to_string(), OsString::from("1")),
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

    /// A command that runs `program` (a guest path) through the loader
    /// with the caller's environment plus the engine block. The loader is
    /// exec'd with the guest's own argv (`argv[0]` = `program`) and learns
    /// what to run from `ZD_PROGRAM`, so the kernel's argument area, and
    /// with it `/proc/<pid>/cmdline`, is exactly the guest's.
    pub fn command(&self, program: &str) -> Command {
        let mut command = Command::new(self.loader_path());
        command.arg0(program);
        command.env(engine_env::PROGRAM, program);
        command.envs(self.engine_env());
        command
    }

    /// Run a guest program to completion with a deadline, capturing its
    /// output. Used by the health check and the self-test.
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
                    log::warn!("rootfs: kill after deadline failed: {error}");
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
        log::warn!("rootfs: reading guest output failed: {error}");
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

struct ChildHandle {
    child: Child,
}

impl SpawnHandle for ChildHandle {
    fn wait(&mut self) -> Result<i32> {
        let status = self.child.wait().context("wait for guest process")?;
        Ok(status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|signal| -signal).unwrap_or(-1)
        }))
    }

    fn kill(&mut self) -> Result<()> {
        self.child.kill().context("kill guest process")
    }
}

impl RuntimeProvider for RootfsAdapter {
    fn id(&self) -> RuntimeId {
        RuntimeId::Rootfs
    }

    fn health_check(&self) -> HealthStatus {
        let loader = self.loader_path();
        if !loader.exists() {
            return HealthStatus::Misconfigured {
                reason: format!(
                    "engine loader missing at {} (reinstall the app)",
                    loader.display()
                ),
            };
        }
        if !self.is_installed() {
            return HealthStatus::NotInstalled {
                hint: "Install the Ubuntu rootfs (33 MB download).".to_string(),
            };
        }
        match self.run_captured("/bin/true", &[], None, Duration::from_secs(3)) {
            Ok(run) if run.succeeded() => HealthStatus::Healthy,
            Ok(run) if run.timed_out => HealthStatus::Failed {
                error: "/bin/true did not exit within 3 s".to_string(),
            },
            Ok(run) => HealthStatus::Failed {
                error: format!(
                    "/bin/true exited with {:?}: {}",
                    run.code,
                    run.stderr_text().trim()
                ),
            },
            Err(error) => HealthStatus::Failed {
                error: format!("{error:#}"),
            },
        }
    }

    fn install(&self, progress: &mut dyn ProgressSink) -> Result<()> {
        super::rootfs_install::install(self, &config::nameservers(), progress)
    }

    fn uninstall(&self) -> Result<()> {
        super::rootfs_install::uninstall(self)
    }

    fn spawn(&self, req: SpawnRequest) -> Result<Box<dyn SpawnHandle>> {
        let mut command = self.command(&req.program);
        command.args(&req.args);
        let env: HashMap<String, OsString> = req.env;
        command.env_clear().envs(env).envs(self.engine_env());
        if let Some(cwd) = &req.cwd {
            command.current_dir(cwd);
        }
        let [stdin, stdout, stderr] = req.stdio;
        command
            .stdin(stdio_from(stdin)?)
            .stdout(stdio_from(stdout)?)
            .stderr(stdio_from(stderr)?);
        let child = command.spawn().context("spawn through the engine loader")?;
        Ok(Box::new(ChildHandle { child }))
    }

    fn needs_command_bridge(&self) -> bool {
        // Absolute paths under `<data>` (managed Node, downloaded language
        // servers) are glibc binaries the kernel cannot exec; the bridge
        // routes them through `zd-exec`, a farm entry that is the loader.
        true
    }

    fn environment_root(&self) -> PathBuf {
        self.data_path.clone()
    }

    fn list_binaries(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for sub in [
            "usr/bin",
            "usr/local/bin",
            "usr/sbin",
            "bin",
            "sbin",
            "root/.cargo/bin",
            "root/.local/bin",
        ] {
            let dir = self.config.rootfs_dir.join(sub);
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

    fn env_for_zed_process(&self, data_path: &Path) -> Vec<(String, util::env::EnvOp)> {
        // Zed's own process keeps the host shape (HOME, PATH with the farm
        // first); the engine block rides along so every child spawned
        // from it, through the farm or the bridge, reaches the loader
        // configured without per-call-site work.
        let mut ops = crate::env::baseline_env(data_path);
        for (key, value) in self.engine_env() {
            ops.push((key, util::env::EnvOp::Set(value)));
        }
        ops.push((
            "SHELL".into(),
            util::env::EnvOp::Set(self.loader_path().into_os_string()),
        ));
        ops
    }

    fn terminal_shell(&self, _data_path: &Path) -> Option<TerminalShell> {
        // The loader under its own name with a leading flag runs the
        // default shell with that flag: `/bin/bash -l`.
        Some(TerminalShell {
            program: self.loader_path(),
            args: vec!["-l".to_string()],
        })
    }

    fn farm_link_target(&self) -> PathBuf {
        PathBuf::from("../bin").join(LOADER_LINK_NAME)
    }

    fn workspace_root(&self, data_path: &Path) -> Option<PathBuf> {
        Some(data_path.join("home"))
    }
}

/// Duplicate a caller fd into a `Stdio`, leaving the caller's copy open.
fn stdio_from(fd: std::os::fd::RawFd) -> Result<Stdio> {
    let duplicated = unsafe { libc_dup(fd) };
    anyhow::ensure!(duplicated >= 0, "dup({fd}) failed");
    Ok(unsafe { Stdio::from_raw_fd(duplicated) })
}

unsafe fn libc_dup(fd: std::os::fd::RawFd) -> std::os::fd::RawFd {
    unsafe extern "C" {
        fn dup(fd: std::os::fd::RawFd) -> std::os::fd::RawFd;
    }
    unsafe { dup(fd) }
}
