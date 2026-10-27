//! `runtime.toml` schema + the resolved-config struct downstream uses.
//!
//! On disk the file looks like:
//!
//! ```toml
//! # Active adapter. One of: "chroot", "bootstrap", "external_termux"
//! [runtime]
//! type = "chroot"
//!
//! # Per-adapter sections. Only the one matching `runtime.type` is used.
//! [chroot]
//! root = "/data/local/nhsystem/kali-arm64"
//! home_bind = "/zed"
//! spawnd_socket = "/dev/socket/zd-spawn"
//! su_path = "/product/bin/su"
//!
//! [bootstrap]
//! prefix = "/data/data/com.zdroid/files/usr"
//! proot_rootfs = ""  # empty = bare mode
//! release_repo = "Dylanmurzello/zdroid-bootstrap"
//!
//! [external_termux]
//! package = "com.termux"
//! prefix = "/data/data/com.termux/files/usr"
//! ```
//!
//! Loading flow:
//!
//! ```ignore
//! let raw = RuntimeFile::load(&conf_path)?;          // serde-parse
//! let resolved = raw.resolve()?;                      // pick the active section
//! let provider = adapters::for_config(&resolved)?;    // factory
//! provider.spawn(req)?;
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Stable identifier for each adapter. Stored as a string in
/// `runtime.toml` (`[runtime] type = "chroot"`) and surfaced in the
/// settings UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeId {
    Chroot,
    Bootstrap,
    ExternalTermux,
}

impl RuntimeId {
    /// Display name shown in the picker UI. Distinct from the on-disk
    /// snake_case form so we can change wording without invalidating
    /// existing configs.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Chroot => "Chroot rootfs",
            Self::Bootstrap => "Zdroid Bootstrap",
            Self::ExternalTermux => "Existing Termux app",
        }
    }
}

/// Top-level config file shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeFile {
    pub runtime: RuntimeSelection,

    #[serde(default)]
    pub chroot: Option<ChrootConfig>,
    #[serde(default)]
    pub bootstrap: Option<BootstrapConfig>,
    #[serde(default)]
    pub external_termux: Option<ExternalTermuxConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSelection {
    #[serde(rename = "type")]
    pub kind: RuntimeId,
}

/// Chroot-adapter config. Deserialized from `[chroot]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChrootConfig {
    /// Filesystem path of the rootfs root, host-side.
    pub root: PathBuf,
    /// Path inside the rootfs that bind-mounts Zdroid's home dir. The
    /// translation layer maps `~/projects/foo` (host) ↔ `/zed/projects/foo`
    /// (chroot) using this.
    pub home_bind: PathBuf,
    /// Abstract or filesystem socket where `zd-spawnd` listens. The
    /// chroot adapter connects here per spawn instead of paying Magisk
    /// `su` mediation cost.
    pub spawnd_socket: PathBuf,
    /// Magisk's su binary, used as a fallback if `zd-spawnd` is not
    /// reachable (e.g. during first install before the Magisk module
    /// has run, or for one-off setup operations).
    pub su_path: PathBuf,
}

/// Bootstrap-adapter config. Deserialized from `[bootstrap]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapConfig {
    /// Where the bootstrap is installed. Matches the historical
    /// Termux-flavored `$PREFIX`.
    pub prefix: PathBuf,
    /// Optional proot rootfs path. Empty / unset = bare termux mode
    /// (direct exec via `$PREFIX/bin/<name>`). Set = wrap each spawn in
    /// `proot -r <rootfs> -- <target>`.
    #[serde(default)]
    pub proot_rootfs: Option<PathBuf>,
    /// `owner/repo` slug of the GitHub repo that publishes
    /// `bootstrap-aarch64-VERSION.tar.zst` releases. The adapter pulls
    /// from `releases/latest` for upgrade detection.
    pub release_repo: String,
}

/// External-Termux-adapter config. Deserialized from `[external_termux]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalTermuxConfig {
    /// Termux's Android package id. Usually `com.termux`; some forks
    /// use `com.termux.x11` etc.
    pub package: String,
    /// Termux's `$PREFIX` (host-readable from Zdroid's app context only
    /// if `sharedUserId` is set up; otherwise this is informational and
    /// commands are executed via `RunCommandService` intent).
    pub prefix: PathBuf,
}

/// Resolved config — the active adapter section flattened so downstream
/// code never has to ask "which kind is this".
#[derive(Debug, Clone)]
pub enum ResolvedConfig {
    Chroot(ChrootConfig),
    Bootstrap(BootstrapConfig),
    ExternalTermux(ExternalTermuxConfig),
}

impl ResolvedConfig {
    /// Identifier for the active adapter.
    pub fn id(&self) -> RuntimeId {
        match self {
            Self::Chroot(_) => RuntimeId::Chroot,
            Self::Bootstrap(_) => RuntimeId::Bootstrap,
            Self::ExternalTermux(_) => RuntimeId::ExternalTermux,
        }
    }
}

/// Per-adapter config struct. Useful for settings UI to enumerate /
/// edit individual fields without caring which is active.
#[derive(Debug, Clone)]
pub enum AdapterConfig {
    Chroot(ChrootConfig),
    Bootstrap(BootstrapConfig),
    ExternalTermux(ExternalTermuxConfig),
}

impl RuntimeFile {
    /// Construct a `RuntimeFile` whose `[runtime] type =` is set to
    /// `id` and whose only populated section is the matching one,
    /// using on-device defaults. Used by the picker UI to write a
    /// fresh `runtime.toml` when the user selects an adapter for the
    /// first time.
    pub fn with_defaults(id: RuntimeId) -> Self {
        let runtime = RuntimeSelection { kind: id };
        match id {
            RuntimeId::Chroot => Self {
                runtime,
                chroot: Some(ChrootConfig {
                    root: PathBuf::from("/data/local/nhsystem/kali-arm64"),
                    home_bind: PathBuf::from("/zed"),
                    spawnd_socket: PathBuf::from("/data/data/com.zdroid/files/run/zd-spawn"),
                    su_path: PathBuf::from("/product/bin/su"),
                }),
                bootstrap: None,
                external_termux: None,
            },
            RuntimeId::Bootstrap => Self {
                runtime,
                chroot: None,
                bootstrap: Some(BootstrapConfig {
                    prefix: PathBuf::from("/data/data/com.zdroid/files/usr"),
                    proot_rootfs: None,
                    release_repo: "Dylanmurzello/zdroid-bootstrap".into(),
                }),
                external_termux: None,
            },
            RuntimeId::ExternalTermux => Self {
                runtime,
                chroot: None,
                bootstrap: None,
                external_termux: Some(ExternalTermuxConfig {
                    package: "com.termux".into(),
                    prefix: PathBuf::from("/data/data/com.termux/files/usr"),
                }),
            },
        }
    }

    /// Read + parse `runtime.toml` from `path`. Returns `Ok(None)` if
    /// the file doesn't exist (first-launch state); errors only on
    /// parse / I/O failures.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(Some(toml::from_str(&raw)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically serialize and write `runtime.toml` to `path`. Writes
    /// to a sibling `path.tmp`, then renames into place — so a partial
    /// write doesn't leave a half-formed config that breaks the
    /// adapter dispatch on next launch.
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serialized = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, serialized)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Pick the active section based on `runtime.type` and return it
    /// as a [`ResolvedConfig`]. Errors if the matching section is
    /// missing — `runtime.toml` declared chroot but no `[chroot]`
    /// block exists, etc.
    pub fn resolve(self) -> anyhow::Result<ResolvedConfig> {
        match self.runtime.kind {
            RuntimeId::Chroot => self
                .chroot
                .map(ResolvedConfig::Chroot)
                .ok_or_else(|| anyhow::anyhow!("runtime.type = chroot but no [chroot] section")),
            RuntimeId::Bootstrap => {
                self.bootstrap
                    .map(ResolvedConfig::Bootstrap)
                    .ok_or_else(|| {
                        anyhow::anyhow!("runtime.type = bootstrap but no [bootstrap] section")
                    })
            }
            RuntimeId::ExternalTermux => self
                .external_termux
                .map(ResolvedConfig::ExternalTermux)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "runtime.type = external_termux but no [external_termux] section"
                    )
                }),
        }
    }
}
