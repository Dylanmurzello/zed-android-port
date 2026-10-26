//! Bootstrap adapter — owns a Termux-flavored `$PREFIX` inside Zdroid's
//! own app sandbox.
//!
//! Two operating modes inside the sandbox:
//!
//! 1. **Bare** (default): spawn directly via `$PREFIX/bin/<target>`.
//!    Bionic-linked binaries from the bootstrap. Zero per-spawn cost
//!    over a normal `Command::new`.
//! 2. **Proot**: wrap each spawn in `proot -r <proot_rootfs> -- ...`,
//!    landing inside a glibc rootfs that lives at
//!    `$PREFIX/var/proot/<distro>`. ~10-15ms ptrace overhead per
//!    syscall but full glibc tools.
//!
//! The bootstrap itself (the rebuilt-Termux userland tarball) lives in
//! a separate GitHub repo and is downloaded on `install()`. Editor
//! releases ship without it; the editor's APK doesn't bundle a
//! userland anymore.

use std::path::PathBuf;

use crate::config::{BootstrapConfig, RuntimeId};
use crate::health::{HealthStatus, ProgressSink};
use crate::port::{RuntimeProvider, SpawnHandle, SpawnRequest};

/// Manages our termux-flavored userland inside the Zdroid sandbox.
pub struct BootstrapAdapter {
    config: BootstrapConfig,
}

impl BootstrapAdapter {
    pub fn new(config: BootstrapConfig) -> anyhow::Result<Self> {
        Ok(Self { config })
    }

    /// Resolve the absolute path of a target binary inside the
    /// bootstrap. We look in `$PREFIX/bin` first, then `$PREFIX/sbin`.
    /// Returns None if neither has the target.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    fn resolve_target(&self, program: &str) -> Option<PathBuf> {
        let candidates = [
            self.config.prefix.join("bin").join(program),
            self.config.prefix.join("sbin").join(program),
        ];
        candidates.into_iter().find(|p| p.exists())
    }
}

#[cfg(target_os = "android")]
mod android_impl {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::fd::BorrowedFd;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use anyhow::{Context, Result};

    use crate::config::BootstrapConfig;
    use crate::port::{SpawnHandle, SpawnRequest};

    /// Wraps a `std::process::Child` so the rest of zdroid_runtime can
    /// treat bootstrap-spawned processes like daemon-spawned ones via
    /// the [`SpawnHandle`] trait. The handle holds the child by value;
    /// dropping kills+reaps if not already exited.
    pub(super) struct BootstrapSpawnHandle {
        child: std::process::Child,
    }

    impl SpawnHandle for BootstrapSpawnHandle {
        fn wait(&mut self) -> Result<i32> {
            let status = self.child.wait().context("wait for bootstrap child")?;
            // Termination by signal: surface as -signum to match the
            // chroot adapter's convention.
            if let Some(code) = status.code() {
                Ok(code)
            } else {
                Ok(-1)
            }
        }

        fn kill(&mut self) -> Result<()> {
            self.child.kill().context("kill bootstrap child")
        }
    }

    /// Build a `Command` for the request, applying cwd/env/stdio. Used
    /// by both bare and proot dispatch (the proot path just exec's
    /// proot with the original target as a tail arg).
    fn build_base_command(
        program: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
        env: &HashMap<String, OsString>,
        stdio: [std::os::fd::RawFd; 3],
    ) -> Result<Command> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        // Replace env entirely with what the caller asked for, plus
        // baseline values the bootstrap expects (PATH, HOME, TERM)
        // unless the caller set them.
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }

        // Dup the caller's stdio fds into owned descriptors so
        // Command can take them. Without dup the original fds would be
        // moved into the child and closed when our request goes out of
        // scope, breaking the caller's view of stdin/stdout/stderr.
        for (i, fd) in stdio.iter().enumerate() {
            // SAFETY: caller asserts the fd is valid for the duration
            // of the spawn; we dup immediately so nothing relies on
            // the source fd staying live past this call.
            let owned = unsafe { BorrowedFd::borrow_raw(*fd) }
                .try_clone_to_owned()
                .with_context(|| format!("dup fd {} (stdio[{}])", fd, i))?;
            match i {
                0 => {
                    cmd.stdin(Stdio::from(owned));
                }
                1 => {
                    cmd.stdout(Stdio::from(owned));
                }
                2 => {
                    cmd.stderr(Stdio::from(owned));
                }
                _ => unreachable!(),
            }
        }

        Ok(cmd)
    }

    pub(super) fn spawn(
        config: &BootstrapConfig,
        target: &Path,
        req: SpawnRequest,
    ) -> Result<Box<dyn SpawnHandle>> {
        let mut cmd = match config.proot_rootfs.as_deref() {
            None => build_base_command(target, &req.args, req.cwd.as_deref(), &req.env, req.stdio)?,
            Some(proot_rootfs) => {
                // Wrap in proot: invoke `$PREFIX/bin/proot -r <rootfs>
                // -- <target> <args...>` with stdio + env passed through.
                let proot = config.prefix.join("bin").join("proot");
                if !proot.exists() {
                    anyhow::bail!(
                        "proot mode requested but {} not found. Install proot in the bootstrap or switch to bare mode.",
                        proot.display()
                    );
                }
                let mut proot_args = vec![
                    OsString::from("-r"),
                    proot_rootfs.as_os_str().to_owned(),
                    OsString::from("-b"),
                    OsString::from("/data/data/com.zdroid/files/home:/zed"),
                    OsString::from("-b"),
                    OsString::from("/storage/emulated/0:/sdcard"),
                    OsString::from("--"),
                    target.as_os_str().to_owned(),
                ];
                proot_args.extend(req.args.iter().cloned());
                build_base_command(&proot, &proot_args, req.cwd.as_deref(), &req.env, req.stdio)?
            }
        };

        let child = cmd.spawn().with_context(|| {
            format!(
                "spawn {} (bootstrap mode {})",
                target.display(),
                if config.proot_rootfs.is_some() {
                    "proot"
                } else {
                    "bare"
                },
            )
        })?;

        Ok(Box::new(BootstrapSpawnHandle { child }))
    }
}

impl RuntimeProvider for BootstrapAdapter {
    fn id(&self) -> RuntimeId {
        RuntimeId::Bootstrap
    }

    fn health_check(&self) -> HealthStatus {
        let bash = self.config.prefix.join("bin").join("bash");
        if !bash.exists() {
            return HealthStatus::NotInstalled {
                hint: format!(
                    "{} missing — bootstrap is not installed. Run install() to download it from {}.",
                    bash.display(),
                    self.config.release_repo,
                ),
            };
        }

        if let Some(proot_rootfs) = &self.config.proot_rootfs {
            // proot mode requires both proot and the rootfs to exist.
            let proot = self.config.prefix.join("bin").join("proot");
            if !proot.exists() {
                return HealthStatus::Misconfigured {
                    reason: format!(
                        "proot mode configured but {} not found. Install proot in the bootstrap or unset proot_rootfs.",
                        proot.display(),
                    ),
                };
            }
            if !proot_rootfs.exists() {
                return HealthStatus::Misconfigured {
                    reason: format!(
                        "proot_rootfs path {} does not exist. Either point at an installed rootfs or unset for bare mode.",
                        proot_rootfs.display(),
                    ),
                };
            }
        }

        HealthStatus::Healthy
    }

    fn install(&self, progress: &mut dyn ProgressSink) -> anyhow::Result<()> {
        // Pull the latest release zip from the configured bootstrap
        // repo, extract into `$PREFIX` atomically, write a version
        // sentinel. Idempotent: re-running when the on-disk version
        // matches the latest release tag is a no-op without touching
        // the network. See `super::bootstrap_install` for the wire
        // protocol + extraction details.
        super::bootstrap_install::install_latest(
            &self.config.prefix,
            &self.config.release_repo,
            progress,
        )
    }

    fn uninstall(&self) -> anyhow::Result<()> {
        // Wipe `$PREFIX/{bin,lib,etc,share,...}` while preserving
        // user-state dirs (`$PREFIX/etc/zd-runtime.toml`,
        // `$PREFIX/home`, `$PREFIX/var/lib/apt` apt cache, etc.) is the
        // intended behavior, but distinguishing "ours" vs "user-
        // created" inside a 240-MB unpack is fragile without a manifest
        // we don't currently ship. Pending until the bootstrap-repo
        // split adds a `MANIFEST.txt` listing extract-owned paths.
        anyhow::bail!(
            "BootstrapAdapter::uninstall pending: needs MANIFEST.txt in the bootstrap zip"
        )
    }

    #[cfg(target_os = "android")]
    fn spawn(&self, req: SpawnRequest) -> anyhow::Result<Box<dyn SpawnHandle>> {
        let target = self.resolve_target(&req.program).ok_or_else(|| {
            anyhow::anyhow!(
                "bootstrap target {} not found in {} (looked in bin/, sbin/)",
                req.program,
                self.config.prefix.display(),
            )
        })?;
        android_impl::spawn(&self.config, &target, req)
    }

    #[cfg(not(target_os = "android"))]
    fn spawn(&self, _req: SpawnRequest) -> anyhow::Result<Box<dyn SpawnHandle>> {
        anyhow::bail!("BootstrapAdapter::spawn is android-only")
    }

    fn environment_root(&self) -> std::path::PathBuf {
        // Bootstrap mode's Zed root is the app data dir Zed already
        // uses — `<app>/files/`. `config.prefix` is `<app>/files/usr/`
        // (the Termux-flavored prefix), so `.parent()` walks up to
        // `<app>/files/`. That keeps existing bootstrap users' state
        // (settings, db, themes, installed extensions, LSP downloads)
        // exactly where they were before per-adapter isolation, so
        // upgrading the APK doesn't orphan their workspace.
        self.config
            .prefix
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.config.prefix.clone())
    }

    fn list_binaries(&self) -> Vec<String> {
        // Walk bootstrap's bin dirs. Same shape as the chroot
        // adapter's walk, just different roots — bootstrap doesn't
        // have a `/usr/` layout, it puts everything directly under
        // `$PREFIX/bin/` (Termux-flavored prefix). `sbin` is rarely
        // populated on Termux but we look anyway.
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for sub in ["bin", "sbin"] {
            let dir = self.config.prefix.join(sub);
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        if let Some(name) = entry.file_name().to_str()
                            && !name.starts_with('.')
                        {
                            names.insert(name.to_string());
                        }
                    }
                }
                Err(e) => {
                    log::debug!(
                        "BootstrapAdapter::list_binaries: skipping {} ({})",
                        dir.display(),
                        e,
                    );
                }
            }
        }
        names.into_iter().collect()
    }

    fn env_for_zed_process(&self, data_path: &std::path::Path) -> Vec<(String, util::env::EnvOp)> {
        // Bootstrap mode keeps the Termux-flavored env shape that
        // historical Zed-on-Android relied on: PREFIX + TERMUX__* +
        // TMPDIR under the bootstrap, PATH front-loaded with
        // .zed/bin / $PREFIX/bin so bootstrap-installed tools win.
        // No zd-runtime symlinks (those are chroot-only); spawns hit
        // $PREFIX/bin directly and inherit Zed-process env.
        use std::ffi::OsString;
        use util::env::EnvOp;

        let prefix = self.config.prefix.clone();
        let termux_home = data_path.join("home");
        let zed_bin = prefix.join(".zed/bin");
        let prefix_bin = prefix.join("bin");
        let existing_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = OsString::new();
        new_path.push(&zed_bin);
        new_path.push(":");
        new_path.push(&prefix_bin);
        new_path.push(":");
        new_path.push(&existing_path);

        // HOME points at the Termux home dir, NOT data_path. Two
        // reasons:
        //   1. Subprocess parity. Every shell, LSP, git, etc. Zed
        //      spawns inherits this env. With HOME=data_path,
        //      `git config --global user.name` reads
        //      `<data>/files/.gitconfig` which doesn't exist — the
        //      user's actual .gitconfig is at `<termux_home>/
        //      .gitconfig` because Termux's profile.d/zed-init.sh
        //      rewrites HOME to termux_home on bash startup. The
        //      integrated terminal sees it; Zed-spawned subprocess
        //      git didn't.
        //   2. Zed's own data dirs are pinned by `paths::set_custom_
        //      data_dir(env_root)` in `gpui_android::lib`, so
        //      config / db / extensions / logs land at
        //      `<env_root>/{config,db,extensions,logs,…}` regardless
        //      of HOME. The few `paths::home_dir()` consumers
        //      (`.ssh/config` in particular) align correctly with
        //      what shells see when both use termux_home.
        vec![
            (
                "HOME".into(),
                EnvOp::Set(termux_home.as_os_str().to_owned()),
            ),
            ("PREFIX".into(), EnvOp::Set(prefix.as_os_str().to_owned())),
            (
                "TERMUX__ROOTFS".into(),
                EnvOp::Set(data_path.as_os_str().to_owned()),
            ),
            (
                "TERMUX__PREFIX".into(),
                EnvOp::Set(prefix.as_os_str().to_owned()),
            ),
            (
                "TERMUX__HOME".into(),
                EnvOp::Set(termux_home.into_os_string()),
            ),
            // Read by our patched dpkg's tarfn.c at extract time. When
            // set and != "com.termux", dpkg rewrites tar entry paths
            // starting with /data/data/com.termux/ to /data/data/
            // <this>/ on the fly, letting `pkg install <upstream-deb>`
            // Just Work with our prefix.
            (
                "TERMUX_APP__PACKAGE_NAME".into(),
                EnvOp::Set(OsString::from("com.zdroid")),
            ),
            (
                "TMPDIR".into(),
                EnvOp::Set(prefix.join("tmp").into_os_string()),
            ),
            ("TERM".into(), EnvOp::Set(OsString::from("xterm-256color"))),
            ("LANG".into(), EnvOp::Set(OsString::from("en_US.UTF-8"))),
            ("COLORTERM".into(), EnvOp::Set(OsString::from("truecolor"))),
            (
                "ZED_BUILD_REMOTE_SERVER".into(),
                EnvOp::Set(OsString::from("never")),
            ),
            // Termux's bootstrap pre-sets LD_PRELOAD via profile.d on
            // bash startup; clearing it on the Zed-Rust process keeps
            // remote-SSH children clean while local Termux shells
            // re-set it themselves where they need the shebang shim.
            ("LD_PRELOAD".into(), EnvOp::Remove),
            ("PATH".into(), EnvOp::Set(new_path)),
            (
                "SHELL".into(),
                EnvOp::Set(prefix.join("bin/bash").into_os_string()),
            ),
        ]
    }

    fn env_for_terminal(&self, data_path: &std::path::Path) -> Vec<(String, util::env::EnvOp)> {
        // PTY spawn replaces inherited env entirely; restore the
        // Termux-flavored vars the integrated terminal expects, plus
        // the libtermux-exec shim, HOME-override, and CA bundle the
        // pre-Phase-3 hardcoded block in terminal.rs used to set.
        use std::ffi::OsString;
        use util::env::EnvOp;

        let prefix = self.config.prefix.clone();
        let termux_home = data_path.join("home");

        let mut ops = vec![
            ("PREFIX".into(), EnvOp::Set(prefix.as_os_str().to_owned())),
            (
                "TERMUX__ROOTFS".into(),
                EnvOp::Set(data_path.as_os_str().to_owned()),
            ),
            (
                "TERMUX__PREFIX".into(),
                EnvOp::Set(prefix.as_os_str().to_owned()),
            ),
            (
                "TERMUX__HOME".into(),
                EnvOp::Set(termux_home.as_os_str().to_owned()),
            ),
            (
                "TERMUX_APP__PACKAGE_NAME".into(),
                EnvOp::Set(OsString::from("com.zdroid")),
            ),
            // Override HOME for the bash subshell: process-side HOME
            // points at data_path (so upstream dirs::home_dir() does
            // not panic), but bash inheriting that makes `~/projects`
            // resolve to a non-existent dir. Aligning with TERMUX__HOME
            // matches Termux convention.
            ("HOME".into(), EnvOp::Set(termux_home.into_os_string())),
            // termux-exec.so hooks execve to translate hardcoded
            // /data/data/com.termux/... shebangs in upstream maintainer
            // scripts to our prefix. Without this, `pkg install` of
            // any upstream package whose preinst has a hardcoded
            // shebang fails with EACCES.
            (
                "LD_PRELOAD".into(),
                EnvOp::Set(OsString::from(
                    "/data/data/com.zdroid/files/usr/lib/libtermux-exec.so",
                )),
            ),
        ];
        let cert_path = prefix.join("etc/tls/cert.pem");
        if cert_path.is_file() {
            // CA bundle for tools that go over HTTPS (cargo, npm,
            // curl). Without this, `cargo metadata` from rust-analyzer
            // dies with "unable to get local issuer certificate" on
            // first crates.io index update.
            ops.push((
                "SSL_CERT_FILE".into(),
                EnvOp::Set(cert_path.as_os_str().to_owned()),
            ));
            ops.push((
                "CURL_CA_BUNDLE".into(),
                EnvOp::Set(cert_path.into_os_string()),
            ));
        }
        ops
    }

    fn terminal_shell(&self, _data_path: &std::path::Path) -> Option<std::path::PathBuf> {
        Some(self.config.prefix.join("bin/bash"))
    }

    fn workspace_root(&self, data_path: &std::path::Path) -> Option<std::path::PathBuf> {
        // <data>/files/home — same as Termux's $TERMUX__HOME and what
        // the chroot adapter publishes. Recent-projects UI and storage
        // hooks read this to know where workspace files live.
        Some(data_path.join("home"))
    }

    fn npm_libtermux_exec_path(&self, _data_path: &std::path::Path) -> Option<std::path::PathBuf> {
        // Bootstrap is the only adapter that ships the bionic-flavored
        // libtermux-exec.so. npm-installed CLIs (Bun-compiled Termux
        // packages: claude, codex, …) LD_PRELOAD this so their
        // hardcoded `/data/data/com.termux/...` shebangs and dlopen
        // calls get path-translated to our $PREFIX.
        Some(self.config.prefix.join("lib/libtermux-exec.so"))
    }
}
