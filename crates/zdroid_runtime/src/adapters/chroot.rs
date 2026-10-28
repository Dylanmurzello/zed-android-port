//! Chroot adapter — talks to `zd-spawnd` over a Unix socket.
//!
//! Per-spawn cost: one connect (~1ms) + one sendmsg with `SCM_RIGHTS`
//! (~1ms) + the daemon's fork/chroot/exec (~3ms). About 5ms total
//! versus ~200ms for the Magisk-su-mediated path, plus no serialized
//! queue contention at scale.
//!
//! See [`zdroid-spawnd/PROTOCOL.md`](https://github.com/Dylanmurzello/zdroid-spawnd/blob/main/PROTOCOL.md) for the wire
//! format. tl;dr: header with magic + version + flags + lengths,
//! followed by prog/cwd/argv/envp byte streams, then a final
//! `sendmsg` with one dummy byte + `SCM_RIGHTS` carrying stdio fds.

use crate::config::{ChrootConfig, RuntimeId};
use crate::health::{HealthStatus, ProgressSink};
use crate::port::{RuntimeProvider, SpawnHandle, SpawnRequest};

/// GitHub releases page for the `zdroid-spawnd` Magisk module — the
/// thing the chroot adapter needs running to function. Surfaced in
/// health-check hints and in the runtime-picker UI when the daemon
/// socket is missing, so the user has a one-click path from "this
/// adapter doesn't work" to "here's how to fix it".
///
/// `releases/latest` (not a pinned tag) so we don't bit-rot every time
/// the module ships a patch. Magisk Manager's own update mechanism
/// (`updateJson` in `module.prop`) handles in-place upgrades after
/// first install.
pub const SPAWND_RELEASE_URL: &str =
    "https://github.com/Dylanmurzello/zdroid-spawnd/releases/latest";

/// Wire-protocol magic; matches `MAGIC` in `zd-spawnd.c`. ASCII "ZDSP"
/// little-endian.
#[allow(dead_code)]
const MAGIC: u32 = 0x5A445350;

/// Wire-protocol version. Bumped only on incompatible changes.
#[allow(dead_code)]
const VERSION: u32 = 1;

/// `flags` bit 0: this spawn is replacing an interactive terminal,
/// daemon should `setsid + TIOCSCTTY` so the inner shell can do job
/// control. Non-tty spawns (LSPs, git) leave this clear.
#[allow(dead_code)]
const FLAG_INTERACTIVE: u32 = 1 << 0;

/// Connects to `zd-spawnd` per spawn. The daemon holds the elevated
/// chroot context so we skip Magisk's su mediation queue.
pub struct ChrootAdapter {
    // Stored unconditionally so the field is always part of the struct
    // shape; only the android-cfg paths actually read it. The dead-code
    // lint is suppressed for non-android builds where install/spawn are
    // unimplemented.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    config: ChrootConfig,
}

impl ChrootAdapter {
    pub fn new(config: ChrootConfig) -> anyhow::Result<Self> {
        Ok(Self { config })
    }
}

#[cfg(target_os = "android")]
mod android_impl {
    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    use anyhow::{Context, Result};
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};

    use crate::config::ChrootConfig;
    use crate::port::{SpawnHandle, SpawnRequest};

    use super::{FLAG_INTERACTIVE, MAGIC, VERSION};

    pub(super) fn connect(socket: &Path) -> Result<UnixStream> {
        UnixStream::connect(socket)
            .with_context(|| format!("connect zd-spawnd at {}", socket.display()))
    }

    pub(super) fn send_request(
        conn: &mut UnixStream,
        config: &ChrootConfig,
        req: &SpawnRequest,
    ) -> Result<()> {
        // Symmetric bind (zd-spawnd v1.1.6+): host's
        // `/data/data/com.zdroid/files` is bound onto the same path
        // inside the chroot, and `/data/user/0/com.zdroid ->
        // /data/data/com.zdroid` is a symlink in the rootfs. Any
        // host path Zed produces resolves to the same inode whether
        // the resolver runs on bionic or inside the chroot. No
        // path translation needed for cwd, argv, or env values.
        //
        // Sanitize env at the boundary anyway: callers may carry a
        // `PATH=…/zd-runtime:…` shaped for host bionic, but the
        // chrooted child wants kali's `PATH=/usr/bin:/usr/sbin:...`.
        // sanitize_env_for_chroot strips the host-shaped env and
        // substitutes chroot-native defaults + INIT_PWD for the
        // NetHunter profile.d hook.
        let env = sanitize_env_for_chroot(config, &req.env, req.cwd.as_deref());

        // zd-runtime paths are the ONE translation that survives the
        // v1.1.6 symmetric bind. zd-runtime/<name> symlinks at
        // `<data>/files/zd-runtime/` point at host's `../bin/zd-exec`
        // — a bionic-linked binary. The bind mount makes the SYMLINK
        // resolvable inside the chroot, but the dynamic loader the
        // symlink target needs (`/system/bin/linker64`) doesn't
        // exist inside the kali rootfs (kali has glibc's
        // `/lib/ld-linux-aarch64.so.1`). So executing the resolved
        // path inside the chroot fails with ENOENT from the loader
        // even though every byte of the path is valid.
        //
        // Fix: strip the zd-runtime prefix to the bare program name
        // so the chroot's own PATH lookup resolves `<name>` against
        // its native `/usr/bin/<name>`. The host-bridge concept stops
        // at the chroot boundary; inside, it's just `java` not
        // `…/zd-runtime/java`.
        let program_translated = strip_zd_runtime(&req.program);
        let args_translated: Vec<OsString> = req
            .args
            .iter()
            .map(|a| {
                let s = a.to_string_lossy();
                let stripped = strip_zd_runtime(&s);
                if stripped == s {
                    a.clone()
                } else {
                    OsString::from(stripped)
                }
            })
            .collect();

        let prog_bytes = program_translated.as_bytes();
        let cwd_bytes: Vec<u8> = req
            .cwd
            .as_deref()
            .map(|p| p.as_os_str().as_bytes().to_vec())
            .unwrap_or_default();

        let argc = args_translated.len() as u32;
        let envc = env.len() as u32;
        let flags = if req.interactive { FLAG_INTERACTIVE } else { 0 };

        // Header: 7 × u32 little-endian.
        let header = [
            MAGIC,
            VERSION,
            flags,
            prog_bytes.len() as u32,
            cwd_bytes.len() as u32,
            argc,
            envc,
        ];
        for word in header {
            conn.write_all(&word.to_le_bytes())?;
        }

        // prog + cwd as raw bytes (no length prefix; lengths are in header).
        conn.write_all(prog_bytes)?;
        conn.write_all(&cwd_bytes)?;

        // argv: each entry length-prefixed. Translated above.
        for arg in &args_translated {
            let bytes = arg.as_bytes();
            conn.write_all(&(bytes.len() as u32).to_le_bytes())?;
            conn.write_all(bytes)?;
        }

        // envp: KEY=VALUE strings, length-prefixed.
        for (key, value) in &env {
            let entry = encode_env_entry(key, value);
            conn.write_all(&(entry.len() as u32).to_le_bytes())?;
            conn.write_all(&entry)?;
        }

        // Stdio fds via SCM_RIGHTS. The daemon's recvmsg expects
        // exactly one byte of regular data accompanying the ancillary,
        // so we send a single dummy byte alongside.
        let dummy = [0u8; 1];
        let iov = [std::io::IoSlice::new(&dummy)];
        let cmsgs = [ControlMessage::ScmRights(&req.stdio)];
        sendmsg::<()>(conn.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
            .context("sendmsg with SCM_RIGHTS for stdio fds")?;

        Ok(())
    }

    /// Build the on-wire `KEY=VALUE` byte sequence. Values may contain
    /// arbitrary bytes (paths with non-UTF8 names, etc.); we don't
    /// re-encode them.
    fn encode_env_entry(key: &str, value: &OsString) -> Vec<u8> {
        let value_bytes = value.as_bytes();
        let mut buf = Vec::with_capacity(key.len() + 1 + value_bytes.len());
        buf.extend_from_slice(key.as_bytes());
        buf.push(b'=');
        buf.extend_from_slice(value_bytes);
        buf
    }

    /// Build the env that the chrooted child will run under. Pulls a
    /// small allow-list of display-related vars from the caller (TERM,
    /// COLORTERM, LANG, …) and pins everything else to chroot-native
    /// defaults. Anything Android-sandbox-specific (PATH pointing at
    /// $PREFIX/bin/, TERMUX__*, ZED_*) is dropped — those paths don't
    /// resolve inside the rootfs and would silently break exec / shell
    /// startup.
    ///
    /// Sets HOME=/root and USER=root explicitly. Empirical finding:
    /// bash does NOT do `getpwuid(uid)` to fill HOME on its own — it
    /// expects HOME to be in the inherited env, the way login(1) /
    /// sshd / a desktop session manager would set it. With HOME unset,
    /// `~/.local/bin` in the chrooted .profile expands to `/.local/bin`
    /// which never exists, so user-installed tools (claude, pip --user
    /// installs, cargo binaries) silently disappear from PATH. /root
    /// is hardcoded because this adapter is debian-rootfs-shaped: uid 0
    /// → /root in /etc/passwd. (Don't set HOME=home_bind — that would
    /// make bash source `<home_bind>/.bashrc` instead of
    /// `/root/.bashrc`, losing the kali prompt + aliases + actual user
    /// dotfiles.)
    ///
    /// Sets `INIT_PWD=<chroot_cwd>` when a translated cwd is supplied.
    /// NetHunter ships `/etc/profile.d/init-pwd.sh` which does
    /// `cd "$INIT_PWD"` if the var is set and the dir exists; combined
    /// with the customize.sh patch that gates `/root/.bash_profile`'s
    /// `cd /root` and `cd ~` on the same var, this is what makes a
    /// chrooted login shell actually land at the project path instead
    /// of bouncing to /root.
    pub(super) fn sanitize_env_for_chroot(
        _config: &ChrootConfig,
        caller_env: &std::collections::HashMap<String, OsString>,
        init_pwd: Option<&Path>,
    ) -> Vec<(String, OsString)> {
        // Display / locale vars worth carrying across the boundary so
        // the inner shell renders correctly. Add to this list cautiously
        // — anything path-shaped is an exec-failure waiting to happen.
        //
        // SSH_ASKPASS + SSH_ASKPASS_REQUIRE: when Zed's "Open Remote"
        // spawns ssh, it sets these so ssh delegates password prompts
        // to our Unix-socket relay binary instead of opening /dev/tty
        // (which doesn't exist for the subprocess). The script path
        // SSH_ASKPASS points at is in app-private storage that the
        // symmetric bind-mount makes visible inside the chroot at the
        // same absolute path. Stripping these here was the root cause
        // of `Permission denied / Too many authentication failures`
        // when sshd offered password auth and ssh fell back to a
        // non-existent /dev/tty.
        //
        // SSH_AUTH_SOCK: same reason — if the caller set up an agent
        // socket, the chroot-side ssh needs to find it.
        const PASSTHROUGH: &[&str] = &[
            "TERM",
            "COLORTERM",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TZ",
            "DISPLAY",
            "SSH_ASKPASS",
            "SSH_ASKPASS_REQUIRE",
            "SSH_AUTH_SOCK",
        ];

        let mut env: Vec<(String, OsString)> = Vec::new();

        // Bootstrap PATH so `execvpe(bash)` succeeds. Bash itself, on
        // interactive startup, sources `/etc/profile` and `~/.bashrc`,
        // which typically PREPEND user-installed tool dirs (e.g.
        // `~/.npm-global/bin`, `~/.cargo/bin`) — those win over our
        // bootstrap PATH for any binary the user installed via npm /
        // cargo / etc. Matches what `getconf PATH` returns in fresh
        // debian.
        env.push((
            "PATH".to_string(),
            OsString::from("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
        ));

        // HOME / USER: see docstring. Fixed values for the debian-style
        // rootfs the chroot adapter targets.
        env.push(("HOME".to_string(), OsString::from("/root")));
        env.push(("USER".to_string(), OsString::from("root")));
        env.push(("LOGNAME".to_string(), OsString::from("root")));

        // Carry display vars across.
        for key in PASSTHROUGH {
            if let Some(value) = caller_env.get(*key) {
                env.push((key.to_string(), value.clone()));
            }
        }

        // INIT_PWD: read by NetHunter's /etc/profile.d/init-pwd.sh.
        // The patched .bash_profile also gates its own `cd /root` /
        // `cd ~` on this — set means "Zdroid asked for a specific
        // landing dir, leave it alone".
        if let Some(pwd) = init_pwd {
            env.push(("INIT_PWD".to_string(), pwd.as_os_str().to_owned()));
        }

        env
    }

    // Path-translation helpers (translate_arg_for_chroot,
    // translate_cwd_for_chroot, APP_HOMES) were deleted in zd-spawnd
    // v1.1.6. The symmetric bind-mount (host's `/data/data/com.zdroid/
    // files` onto the same path inside the chroot, plus the
    // `/data/user/0/com.zdroid -> /data/data/com.zdroid` alias
    // symlink in the rootfs) makes host paths resolve identically
    // inside the chroot, so no translation is needed for that class.
    //
    // ONE translation survives: zd-runtime paths. zd-runtime/<name>
    // symlinks at `<data>/files/zd-runtime/` point at host's bionic
    // `<data>/files/bin/zd-exec` binary. The bind mount makes the
    // symlink resolvable inside the chroot, but the loader the binary
    // needs (`/system/bin/linker64`) doesn't exist inside the kali
    // rootfs — kali has glibc's `/lib/ld-linux-aarch64.so.1`. So
    // executing the path inside the chroot fails with ENOENT from the
    // loader. We strip zd-runtime paths to bare program names so the
    // chroot's own PATH lookup resolves them against `/usr/bin/<name>`.

    /// Host-side zd-runtime prefixes that need stripping at the chroot
    /// boundary. Listed in both `/data/data/<pkg>` and `/data/user/0/<pkg>`
    /// forms because either can appear in resolver output. Phase 4 of
    /// the Termux-divestment refactor relocated this off `$PREFIX/usr/`
    /// to the bare `<data>/files/zd-runtime/`.
    const ZD_RUNTIME_DIRS: &[&str] = &[
        "/data/data/com.zdroid/files/zd-runtime/",
        "/data/user/0/com.zdroid/files/zd-runtime/",
    ];

    /// If `s` is a zd-runtime path (starts with one of [`ZD_RUNTIME_DIRS`]
    /// and contains no `/` after the prefix), return the bare program
    /// name. Otherwise return `s` unchanged. See module-level comment
    /// above for why this rewrite is necessary even after the symmetric
    /// bind mount.
    pub(super) fn strip_zd_runtime(s: &str) -> String {
        for prefix in ZD_RUNTIME_DIRS {
            if let Some(rest) = s.strip_prefix(prefix) {
                if !rest.contains('/') {
                    return rest.to_string();
                }
            }
        }
        s.to_string()
    }

    /// Read the daemon's `response_spawned` (4 × u32). Returns the
    /// negotiated child PID (daemon-internal) or surfaces an error
    /// reflecting the daemon's reported errno.
    pub(super) fn read_spawned_response(conn: &mut UnixStream) -> Result<u32> {
        let mut buf = [0u8; 16];
        conn.read_exact(&mut buf)
            .context("read response_spawned from daemon")?;
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let status = i32::from_le_bytes(buf[8..12].try_into().unwrap());
        let pid = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        if magic != MAGIC {
            anyhow::bail!("response_spawned bad magic: {magic:#x}");
        }
        if version != VERSION {
            anyhow::bail!("response_spawned version mismatch: {version}");
        }
        if status < 0 {
            let errno = -status;
            anyhow::bail!(
                "daemon refused spawn: errno={} ({})",
                errno,
                nix::errno::Errno::from_raw(errno).desc(),
            );
        }
        Ok(pid)
    }

    /// Read `response_exited` (3 × u32). Returns the exit code, with
    /// negative values representing terminating signals (e.g. -9 for
    /// SIGKILL).
    pub(super) fn read_exited_response(conn: &mut UnixStream) -> Result<i32> {
        let mut buf = [0u8; 12];
        conn.read_exact(&mut buf)
            .context("read response_exited from daemon")?;
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let exit_code = i32::from_le_bytes(buf[8..12].try_into().unwrap());
        if magic != MAGIC {
            anyhow::bail!("response_exited bad magic: {magic:#x}");
        }
        if version != VERSION {
            anyhow::bail!("response_exited version mismatch: {version}");
        }
        Ok(exit_code)
    }

    pub(super) struct ChrootSpawnHandle {
        pub conn: UnixStream,
        /// Cached on first `wait()` call so subsequent calls are
        /// idempotent and don't try a second `read_exited_response`.
        pub cached_exit_code: Option<i32>,
    }

    impl SpawnHandle for ChrootSpawnHandle {
        fn wait(&mut self) -> Result<i32> {
            if let Some(code) = self.cached_exit_code {
                return Ok(code);
            }
            let code = read_exited_response(&mut self.conn)?;
            self.cached_exit_code = Some(code);
            Ok(code)
        }
    }

    pub(super) fn spawn(config: &ChrootConfig, req: SpawnRequest) -> Result<Box<dyn SpawnHandle>> {
        let mut conn = connect(&config.spawnd_socket)?;
        send_request(&mut conn, config, &req)?;
        let _pid = read_spawned_response(&mut conn)?;
        Ok(Box::new(ChrootSpawnHandle {
            conn,
            cached_exit_code: None,
        }))
    }

    pub(super) fn health_check(config: &ChrootConfig) -> super::HealthStatus {
        match UnixStream::connect(&config.spawnd_socket) {
            Ok(_) => super::HealthStatus::Healthy,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                super::HealthStatus::NotInstalled {
                    hint: format!(
                        "zd-spawnd daemon not running. Install the Magisk module from {}, then reboot.",
                        super::SPAWND_RELEASE_URL,
                    ),
                }
            }
            Err(e) => super::HealthStatus::Failed {
                error: format!(
                    "connect zd-spawnd at {}: {e}",
                    config.spawnd_socket.display()
                ),
            },
        }
    }
}

impl RuntimeProvider for ChrootAdapter {
    fn id(&self) -> RuntimeId {
        RuntimeId::Chroot
    }

    #[cfg(target_os = "android")]
    fn health_check(&self) -> HealthStatus {
        android_impl::health_check(&self.config)
    }

    #[cfg(not(target_os = "android"))]
    fn health_check(&self) -> HealthStatus {
        HealthStatus::NotInstalled {
            hint: "ChrootAdapter is android-only; this is a non-android build.".into(),
        }
    }

    fn install(&self, _progress: &mut dyn ProgressSink) -> anyhow::Result<()> {
        anyhow::bail!(
            "ChrootAdapter::install: install the zdroid-spawnd Magisk module \
             to start the daemon. Auto-install of Magisk modules from within \
             the app is not yet supported."
        )
    }

    #[cfg(target_os = "android")]
    fn spawn(&self, req: SpawnRequest) -> anyhow::Result<Box<dyn SpawnHandle>> {
        android_impl::spawn(&self.config, req)
    }

    #[cfg(not(target_os = "android"))]
    fn spawn(&self, _req: SpawnRequest) -> anyhow::Result<Box<dyn SpawnHandle>> {
        anyhow::bail!("ChrootAdapter::spawn is android-only")
    }

    fn needs_command_bridge(&self) -> bool {
        // Chroot crosses a namespace boundary: an absolute host path
        // under `environment_root()` only resolves correctly when the
        // spawn lands inside the chroot via `zd-exec` → `zd-spawnd` →
        // chroot dispatch. PATH for this adapter front-loads the
        // `zd-runtime/` symlink farm so `zd-exec` is kernel-resolvable
        // by short name.
        true
    }

    fn environment_root(&self) -> std::path::PathBuf {
        // Host-side path that becomes Zed's ENTIRE data root when this
        // adapter is active (config, db, logs, extensions, languages,
        // themes — everything). Two requirements:
        //
        //   1. Must live inside the bind-mount source so the same bytes
        //      are reachable inside the chroot. The daemon binds
        //      `/data/data/com.zdroid/files/home` onto `/zed`, so a
        //      file at `<this>/extensions/foo` on host is visible at
        //      `/zed/.zed-env/chroot/extensions/foo` inside the chroot.
        //      The adapter's argv-translation rewrites host paths in
        //      spawn arguments to their chroot-target equivalents so
        //      LSPs and other tools resolve cleanly.
        //
        //   2. Must be a per-adapter subdir, not just `$HOME` itself.
        //      The user's home dir is theirs, not Zdroid's; we
        //      shouldn't drop `languages/`, `extensions/`, `db/` etc.
        //      tree-roots at the top level of their home.
        //
        // Hardcoded to `/data/data/com.zdroid/files/home/.zed-env/
        // chroot`. Lives under `/data/data/com.zdroid/files`, which
        // zd-spawnd v1.1.6+ symmetrically bind-mounts onto the same
        // path inside the chroot — so this exact byte string resolves
        // to the same inode whether the resolver runs on host bionic
        // or inside the chroot. No translation. Future: thread the
        // path through config so the user can pick a non-default root.
        std::path::PathBuf::from("/data/data/com.zdroid/files/home/.zed-env/chroot")
    }

    fn list_binaries(&self) -> Vec<String> {
        // Walk the chroot rootfs's bin dirs and return every entry
        // name. The boot process turns each into a
        // `$PREFIX/zd-runtime/<name>` symlink to `zd-exec`, which is
        // how Zed's `Command::new("java")` PATH lookup finds the
        // chroot's java (or apt-get, or rust-analyzer, or whatever)
        // through the bridge.
        //
        // Order matters in PATH-lookup-equivalent style, but since
        // every entry routes to the same `zd-exec` and `zd-exec`
        // doesn't care about discovery order, we collapse duplicates
        // into a set. The chroot's own internal PATH order applies on
        // the daemon side when execvpe resolves the binary.
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for sub in ["usr/bin", "usr/local/bin", "usr/sbin", "bin", "sbin"] {
            let dir = self.config.root.join(sub);
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
                        "ChrootAdapter::list_binaries: skipping {} ({})",
                        dir.display(),
                        e,
                    );
                }
            }
        }
        names.into_iter().collect()
    }

    fn env_for_zed_process(
        &self,
        data_path: &std::path::Path,
    ) -> Vec<(String, crate::port::EnvOp)> {
        // Chroot mode keeps the Zed-Rust process bionic-clean: no
        // Termux-flavored vars, no $PREFIX/etc.pem CA pointer, no
        // libtermux-exec preload. Anything spawn-target-specific gets
        // injected at the zd-spawnd boundary via
        // `sanitize_env_for_chroot`.
        use crate::port::EnvOp;
        use std::ffi::OsString;

        // Phase 4: zd-exec + zd-runtime live at `<data>/files/bin/` and
        // `<data>/files/zd-runtime/` respectively, not under the
        // bootstrap-flavored `<data>/files/usr/`. Chroot mode has no
        // dependency on the bootstrap layout anymore.
        let zd_runtime = data_path.join("zd-runtime");
        let bin = data_path.join("bin");
        let existing_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = OsString::new();
        new_path.push(&zd_runtime);
        new_path.push(":");
        new_path.push(&bin);
        new_path.push(":");
        new_path.push(&existing_path);

        vec![
            // HOME stays pointed at the app's data root for compat
            // with upstream `dirs::home_dir()` consumers; the chroot
            // adapter's bind-mount makes this path the same inside
            // the rootfs.
            ("HOME".into(), EnvOp::Set(data_path.as_os_str().to_owned())),
            // Generic non-Termux tmp dir under the app sandbox.
            (
                "TMPDIR".into(),
                EnvOp::Set(data_path.join("tmp").into_os_string()),
            ),
            ("TERM".into(), EnvOp::Set(OsString::from("xterm-256color"))),
            ("COLORTERM".into(), EnvOp::Set(OsString::from("truecolor"))),
            ("LANG".into(), EnvOp::Set(OsString::from("en_US.UTF-8"))),
            // Disable the remote-server source-build fallback (see
            // upstream `crates/remote/src/transport.rs`); cross-
            // compiling rust on the device is never viable on Android
            // and forcing the CDN-or-existing-on-remote path keeps
            // the SSH transport behavior the same as desktop Zed.
            (
                "ZED_BUILD_REMOTE_SERVER".into(),
                EnvOp::Set(OsString::from("never")),
            ),
            // No bootstrap libtermux-exec.so in chroot mode; if some
            // ancestor shell leaked LD_PRELOAD, drop it so remote
            // SSH subprocesses don't spam `cannot be preloaded` on
            // every command.
            ("LD_PRELOAD".into(), EnvOp::Remove),
            // PATH: chroot has the zd-runtime symlink farm at the
            // front. Kernel PATH lookup of `Command::new("java")`
            // finds the symlink, exec's zd-exec, which dispatches via
            // zd-spawnd into the rootfs.
            ("PATH".into(), EnvOp::Set(new_path)),
            // SHELL points at zd-exec so the integrated terminal
            // lands inside the rootfs via the same wrapper.
            (
                "SHELL".into(),
                EnvOp::Set(bin.join("zd-exec").into_os_string()),
            ),
        ]
    }

    fn env_for_terminal(&self, _data_path: &std::path::Path) -> Vec<(String, std::ffi::OsString)> {
        // No overlay in chroot mode. The integrated-terminal SHELL is
        // zd-exec; it talks to zd-spawnd which builds the chroot-side
        // env via `sanitize_env_for_chroot`. Anything the host-side
        // PTY env carries is incidental once we cross the boundary.
        Vec::new()
    }

    fn terminal_shell(&self, data_path: &std::path::Path) -> Option<std::path::PathBuf> {
        Some(data_path.join("bin/zd-exec"))
    }

    fn workspace_root(&self, data_path: &std::path::Path) -> Option<std::path::PathBuf> {
        // <data>/files/home is the canonical user-files dir for both
        // chroot and bootstrap modes — zd-spawnd's symmetric bind-
        // mount makes the same path resolve identically inside the
        // chroot, so a project under here is reachable from both
        // sides without translation.
        Some(data_path.join("home"))
    }

    // npm_libtermux_exec_path: default None. Chroot doesn't ship the
    // bionic-flavored libtermux-exec.so; tools spawned inside the
    // rootfs link against glibc and use shebangs that resolve
    // normally.
}
