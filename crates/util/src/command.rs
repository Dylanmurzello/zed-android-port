use std::ffi::OsStr;
#[cfg(not(target_os = "macos"))]
use std::path::Path;
#[cfg(target_os = "android")]
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
pub use darwin::{Child, Command, Stdio};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000_u32;

/// `chdir(2)` in the forked subprocess fails with ENOENT for paths
/// rooted at `/storage/emulated/`, `/sdcard/`, or `/mnt/runtime/` even
/// when the parent process can readdir and open files there. Android's
/// FUSE-emulated external storage gates entry against the requesting
/// process's UID + supplementary groups; the parent's media_rw group
/// membership grants file ops via the FUSE proxy, but the forked
/// subprocess's chdir() syscall is checked through a separate path
/// that returns ENOENT for any external-storage prefix. This breaks
/// every LSP server / build tool spawn whose worktree lives on
/// `/sdcard/projects/`.
///
/// Rewrite the requested CWD to an app-private fallback (Termux home,
/// then app data dir, then /system/bin) when it falls under an
/// external-storage prefix. LSP servers receive the actual project
/// path via the `workspaceFolders` initialize parameter, so
/// rust-analyzer / pyright / etc. still locate the project root
/// correctly; only the kernel-level CWD changes.
#[cfg(target_os = "android")]
fn android_safe_cwd(dir: &Path) -> PathBuf {
    const EXTERNAL_PREFIXES: &[&str] = &[
        "/storage/emulated/",
        "/storage/self/",
        "/sdcard/",
        "/mnt/runtime/",
        "/mnt/user/",
        "/mnt/media_rw/",
    ];
    let path_str = dir.to_string_lossy();
    if EXTERNAL_PREFIXES.iter().any(|p| path_str.starts_with(p)) {
        if let Some(home) = std::env::var_os("TERMUX__HOME")
            .or_else(|| std::env::var_os("HOME"))
        {
            let home_path = PathBuf::from(home);
            if home_path.is_dir() {
                log::debug!(
                    "util::command: rewriting external-storage CWD {:?} -> {:?}",
                    dir,
                    home_path,
                );
                return home_path;
            }
        }
        return PathBuf::from("/system/bin");
    }
    dir.to_path_buf()
}

pub use gpui_util::new_std_command;

/// The zd-exec spawn bridge itself (env-root routing, shebang rewrite,
/// boot-time root registration) lives in `gpui_util` so the sync
/// `new_std_command` path upstream re-exports above carries the Android
/// rewrite too. Re-exported here so app-side callers keep addressing
/// the bridge through `util::command`.
#[cfg(target_os = "android")]
pub use gpui_util::register_environment_root;
#[cfg(target_os = "android")]
use gpui_util::{ZD_EXEC_PROGRAM, detect_env_shebang, env_root_program_path};

pub fn new_command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

#[cfg(not(target_os = "macos"))]
pub type Child = smol::process::Child;

#[cfg(not(target_os = "macos"))]
pub use std::process::Stdio;

#[cfg(not(target_os = "macos"))]
#[derive(Debug)]
pub struct Command(smol::process::Command);

#[cfg(not(target_os = "macos"))]
impl Command {
    #[inline]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        #[cfg(target_os = "windows")]
        {
            use smol::process::windows::CommandExt;
            let mut cmd = smol::process::Command::new(program);
            cmd.creation_flags(CREATE_NO_WINDOW);
            Self(cmd)
        }
        #[cfg(target_os = "android")]
        {
            // Two rewrites, in this order:
            //
            //   1. env-root bridge — if `program` is an absolute path
            //      under the active adapter's `environment_root()`,
            //      rewrite to `zd-exec <program>`. zd-exec dispatches
            //      the spawn into the configured runtime (chroot /
            //      bootstrap / external Termux), so glibc-linked
            //      binaries from inside a chroot rootfs run where
            //      their loader actually exists. See
            //      `env_root_program_path` for the full rationale.
            //
            //   2. shebang rewrite for absolute-path script invocations
            //      that need `/usr/bin/env`. See `detect_env_shebang`
            //      for the full rationale; tl;dr the host has no
            //      `/usr/bin/env`, so we replace the program with the
            //      script's declared interpreter and let kernel PATH
            //      lookup route through `zd-runtime/<interp>` into the
            //      chroot/bootstrap that does have it.
            //
            // The env-root bridge wins when both could apply (e.g. a
            // script that lives inside env_root): routing through
            // zd-exec is the more general fix and the chroot adapter
            // will resolve shebangs natively once the script is exec'd
            // inside the rootfs.
            let program_ref = program.as_ref();
            if let Some(program_path) = env_root_program_path(program_ref) {
                let mut cmd = smol::process::Command::new(ZD_EXEC_PROGRAM);
                cmd.arg(program_path);
                return Self(cmd);
            }
            if let Some((interp, script)) = detect_env_shebang(program_ref) {
                let mut cmd = smol::process::Command::new(interp);
                cmd.arg(script);
                return Self(cmd);
            }
            Self(smol::process::Command::new(program_ref))
        }
        #[cfg(all(not(target_os = "windows"), not(target_os = "android")))]
        Self(smol::process::Command::new(program))
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.0.args(args);
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.0.get_args()
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.0.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.envs(vars);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.0.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.0.env_clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        #[cfg(target_os = "android")]
        {
            self.0.current_dir(android_safe_cwd(dir.as_ref()));
        }
        #[cfg(not(target_os = "android"))]
        {
            self.0.current_dir(dir);
        }
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdin(cfg.into());
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stdout(cfg.into());
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.0.stderr(cfg.into());
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.0.kill_on_drop(kill_on_drop);
        self
    }

    pub fn spawn(&mut self) -> std::io::Result<Child> {
        self.0.spawn()
    }

    pub async fn output(&mut self) -> std::io::Result<std::process::Output> {
        self.0.output().await
    }

    pub async fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.status().await
    }

    pub fn get_program(&self) -> &OsStr {
        self.0.get_program()
    }
}
