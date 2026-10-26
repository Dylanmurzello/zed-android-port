//! `zd-runtime.toml` schema, its on-device location, and the runtime
//! state the editor boots from.
//!
//! On disk:
//!
//! ```toml
//! [runtime]
//! type = "chroot"
//!
//! [chroot]
//! root = "/data/local/nhsystem/kali-arm64"
//! home_bind = "/zed"
//! spawnd_socket = "/data/data/com.zdroid/files/run/zd-spawn"
//! su_path = "/product/bin/su"
//! ```
//!
//! The file lives at `<data>/files/etc/zd-runtime.toml`. Earlier releases
//! wrote it inside the bootstrap prefix (`<data>/files/usr/etc/`), which the
//! bootstrap installer wiped; [`RuntimeState::load`] migrates that file once
//! and reports adapter types this build no longer knows as "not configured"
//! instead of booting on a silent default.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// App files dir when the platform did not hand us one.
pub const DEFAULT_DATA_PATH: &str = "/data/data/com.zdroid/files";

static DATA_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Record the app files dir once at boot so code without access to the
/// `AndroidApp` (picker, helpers) can find the runtime config.
pub fn set_data_path(path: PathBuf) {
    if DATA_PATH.set(path).is_err() {
        log::debug!("zdroid_runtime: data_path already set; ignoring re-set");
    }
}

/// The app files dir recorded by [`set_data_path`], or the default.
pub fn data_path() -> PathBuf {
    DATA_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_PATH))
}

pub fn runtime_toml_path(data_path: &Path) -> PathBuf {
    data_path.join("etc/zd-runtime.toml")
}

/// Pre-migration location inside the bootstrap prefix.
pub fn legacy_runtime_toml_path(data_path: &Path) -> PathBuf {
    data_path.join("usr/etc/zd-runtime.toml")
}

/// Stable identifier for each adapter. Stored as a string in
/// `zd-runtime.toml` (`[runtime] type = "chroot"`) and surfaced in the
/// settings UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeId {
    Chroot,
}

impl RuntimeId {
    /// Display name shown in the picker UI. Distinct from the on-disk
    /// snake_case form so we can change wording without invalidating
    /// existing configs.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Chroot => "Chroot rootfs",
        }
    }
}

/// What `[runtime] type` holds. Ids this build knows deserialize to
/// `Known`; anything else (adapters that were removed, typos) lands in
/// `Legacy` instead of failing the whole parse, so callers can tell the
/// user exactly what they found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RuntimeKind {
    Known(RuntimeId),
    Legacy(String),
}

/// Top-level config file shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeFile {
    pub runtime: RuntimeSelection,

    #[serde(default)]
    pub chroot: Option<ChrootConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSelection {
    #[serde(rename = "type")]
    pub kind: RuntimeKind,
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

/// Resolved config — the active adapter section flattened so downstream
/// code never has to ask "which kind is this".
#[derive(Debug, Clone)]
pub enum ResolvedConfig {
    Chroot(ChrootConfig),
}

impl ResolvedConfig {
    /// Identifier for the active adapter.
    pub fn id(&self) -> RuntimeId {
        match self {
            Self::Chroot(_) => RuntimeId::Chroot,
        }
    }
}

/// Why no adapter is active. Logged at boot and shown by the UI so the
/// user is routed to the picker with a precise message.
#[derive(Debug, Clone)]
pub enum NotConfiguredReason {
    /// No config file at either location: first launch.
    Missing,
    /// The file names an adapter this build does not have (`bootstrap`,
    /// `external_termux`, ...). The user has to pick again.
    Legacy(String),
    /// The file exists but is not valid TOML for the schema.
    Unparseable(String),
    /// The type is known but its section is missing or the adapter could
    /// not be constructed.
    Unresolvable(String),
}

impl std::fmt::Display for NotConfiguredReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "no zd-runtime.toml yet"),
            Self::Legacy(kind) => write!(f, "runtime type {kind:?} is not available in this build"),
            Self::Unparseable(error) => write!(f, "zd-runtime.toml is unreadable: {error}"),
            Self::Unresolvable(error) => write!(f, "{error}"),
        }
    }
}

/// What the editor boots from.
#[derive(Debug, Clone)]
pub enum RuntimeState {
    NotConfigured(NotConfiguredReason),
    Configured(ResolvedConfig),
}

impl RuntimeState {
    /// Read the runtime config, migrating a pre-move file once. Never
    /// errors: every failure becomes a [`NotConfiguredReason`].
    pub fn load(data_path: &Path) -> Self {
        let path = runtime_toml_path(data_path);
        match RuntimeFile::load(&path) {
            Ok(Some(file)) => return Self::from_file(file),
            Ok(None) => {}
            Err(error) => {
                return Self::NotConfigured(NotConfiguredReason::Unparseable(format!(
                    "{}: {error:#}",
                    path.display()
                )));
            }
        }

        let legacy_path = legacy_runtime_toml_path(data_path);
        match RuntimeFile::load(&legacy_path) {
            Ok(Some(file)) => {
                let state = Self::from_file(file.clone());
                if let Self::Configured(_) = &state {
                    match file.save(&path) {
                        Ok(()) => log::info!(
                            "zdroid_runtime: migrated {} -> {}",
                            legacy_path.display(),
                            path.display()
                        ),
                        Err(error) => log::warn!(
                            "zdroid_runtime: could not migrate {} -> {}: {error:#}",
                            legacy_path.display(),
                            path.display()
                        ),
                    }
                }
                state
            }
            Ok(None) => Self::NotConfigured(NotConfiguredReason::Missing),
            Err(error) => Self::NotConfigured(NotConfiguredReason::Unparseable(format!(
                "{}: {error:#}",
                legacy_path.display()
            ))),
        }
    }

    fn from_file(file: RuntimeFile) -> Self {
        match &file.runtime.kind {
            RuntimeKind::Legacy(kind) => {
                Self::NotConfigured(NotConfiguredReason::Legacy(kind.clone()))
            }
            RuntimeKind::Known(_) => match file.resolve() {
                Ok(resolved) => Self::Configured(resolved),
                Err(error) => {
                    Self::NotConfigured(NotConfiguredReason::Unresolvable(format!("{error:#}")))
                }
            },
        }
    }

    /// The active adapter id, if any.
    pub fn id(&self) -> Option<RuntimeId> {
        match self {
            Self::Configured(resolved) => Some(resolved.id()),
            Self::NotConfigured(_) => None,
        }
    }
}

impl RuntimeFile {
    /// Construct a `RuntimeFile` whose `[runtime] type =` is set to
    /// `id` and whose only populated section is the matching one,
    /// using on-device defaults. Used by the picker UI to write a
    /// fresh config when the user selects an adapter.
    pub fn with_defaults(id: RuntimeId) -> Self {
        let runtime = RuntimeSelection {
            kind: RuntimeKind::Known(id),
        };
        match id {
            RuntimeId::Chroot => Self {
                runtime,
                chroot: Some(ChrootConfig {
                    root: PathBuf::from("/data/local/nhsystem/kali-arm64"),
                    home_bind: PathBuf::from("/zed"),
                    spawnd_socket: PathBuf::from("/data/data/com.zdroid/files/run/zd-spawn"),
                    su_path: PathBuf::from("/product/bin/su"),
                }),
            },
        }
    }

    /// Read + parse the config from `path`. Returns `Ok(None)` if the
    /// file doesn't exist; errors only on parse / I/O failures.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(Some(toml::from_str(&raw)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically serialize and write the config to `path`. Writes to a
    /// sibling `path.tmp`, then renames into place, so a partial write
    /// doesn't leave a half-formed config that breaks adapter dispatch
    /// on next launch.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
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
    /// as a [`ResolvedConfig`]. Errors if the type is unknown or the
    /// matching section is missing.
    pub fn resolve(self) -> anyhow::Result<ResolvedConfig> {
        match self.runtime.kind {
            RuntimeKind::Known(RuntimeId::Chroot) => self
                .chroot
                .map(ResolvedConfig::Chroot)
                .ok_or_else(|| anyhow::anyhow!("runtime.type = chroot but no [chroot] section")),
            RuntimeKind::Legacy(kind) => {
                anyhow::bail!("runtime.type = {kind:?} is not available in this build")
            }
        }
    }
}
