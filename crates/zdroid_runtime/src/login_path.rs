//! The login shell's environment for the editor and its guest children.
//!
//! Desktop Zed captures the login shell's environment at startup and sets
//! every variable on its own process (`util::load_login_shell_environment`),
//! which is how the tools mise, asdf or nvm put on PATH reach it. The app
//! runs the same capture with root's login shell, but the editor's process
//! has to keep the environment the runtime gives it, PATH with the farm
//! first and the guest's home in the host's spelling, and the guest view
//! overrides the same names for its children. So the shell's PATH becomes
//! the guest's, in `ZD_GUEST_ENV`, the farm mirrors its folders, and every
//! other variable is set as Zed sets it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use zd_engine_core::{DEFAULT_GUEST_PATH, LIST_SEPARATOR, env as engine_env, path::MAX_PATH};

use crate::Runtime;
use crate::env::EnvOp;

/// The engine holds all of `ZD_GUEST_ENV` in [`MAX_PATH`] bytes and fails
/// every exec when it doesn't fit, and a spawn can add its own PATH entries
/// to it (`zdroid_hooks::guest_env_for_path`), so the PATH keeps a quarter
/// of that free.
const MAX_GUEST_PATH: usize = MAX_PATH / 4 * 3;

static GUEST_PATH: RwLock<Option<String>> = RwLock::new(None);

impl Runtime {
    /// The PATH the editor's guest children get: the login shell's, or the
    /// engine's default before it's loaded.
    pub fn guest_path(&self) -> String {
        GUEST_PATH
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| DEFAULT_GUEST_PATH.to_string())
    }

    /// The changes Zed's `load_login_shell_environment` makes to its
    /// process, for the login shell's `environment`: every variable but
    /// `SHLVL`, as there, and those the runtime sets for the editor
    /// ([`Runtime::env_for_zed_process`]) or the guest
    /// ([`Runtime::guest_env_spec`]). The shell's PATH becomes the guest
    /// PATH instead.
    pub fn env_for_login_shell(
        &self,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Vec<(String, EnvOp)> {
        let (guest_path, ops) = self.login_shell_changes(environment);
        if let Some(guest_path) = guest_path {
            *GUEST_PATH
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guest_path);
        }
        ops
    }

    fn login_shell_changes(
        &self,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> (Option<String>, Vec<(String, EnvOp)>) {
        let mut runtimes: HashSet<String> = self
            .env_for_zed_process()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        runtimes.extend(
            self.guest_env_spec()
                .split(LIST_SEPARATOR)
                .filter_map(|item| item.split_once('='))
                .map(|(name, _)| name.to_string()),
        );
        let mut guest_path = None;
        let mut ops = Vec::new();
        for (name, value) in environment {
            if name == "PATH" {
                guest_path = Some(merge_guest_path(&value));
            // The engine passes its own `ZD_` variables on to the guest's
            // children; they're configuration, not the shell's.
            } else if name != "SHLVL" && !name.starts_with("ZD_") && !runtimes.contains(&name) {
                ops.push((name, EnvOp::Set(value.into())));
            }
        }
        let spec =
            self.guest_env_spec_with_path(guest_path.as_deref().unwrap_or(&self.guest_path()));
        ops.push((engine_env::GUEST_ENV.to_string(), EnvOp::Set(spec.into())));
        (guest_path, ops)
    }

    /// The guest PATH's folders beyond the engine default's, as host paths.
    pub(crate) fn login_binary_dirs(&self) -> Vec<PathBuf> {
        self.login_binary_dirs_on(&self.guest_path())
    }

    /// Folders under a binding (`/sdcard`, `/tmp`, the files dir) aren't
    /// the rootfs's.
    fn login_binary_dirs_on(&self, guest_path: &str) -> Vec<PathBuf> {
        let rootfs = self.rootfs_dir();
        let bound = self.bind_guest_dirs();
        guest_path
            .split(':')
            .filter(|entry| {
                !DEFAULT_GUEST_PATH
                    .split(':')
                    .any(|default| default == *entry)
            })
            .filter(|entry| {
                !bound
                    .iter()
                    .any(|guest| Path::new(entry).starts_with(guest))
            })
            .map(|entry| rootfs.join(entry.trim_start_matches('/')))
            .filter(|dir| dir.is_dir())
            .collect()
    }
}

/// The login shell's absolute PATH entries in its order, then those of the
/// engine default it lacks: the farm mirrors their folders and the loader
/// looks its links up on this PATH. Past [`MAX_GUEST_PATH`], the login
/// shell's last entries are dropped.
fn merge_guest_path(login_path: &str) -> String {
    let defaults: Vec<&str> = DEFAULT_GUEST_PATH.split(':').collect();
    let mut entries: Vec<&str> = Vec::new();
    for entry in login_path.split(':').chain(defaults.iter().copied()) {
        if entry.starts_with('/') && !entry.contains(LIST_SEPARATOR) && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    while entries.join(":").len() > MAX_GUEST_PATH {
        let Some(last) = entries.iter().rposition(|entry| !defaults.contains(entry)) else {
            break;
        };
        log::warn!(
            "runtime: the login PATH is over {MAX_GUEST_PATH} bytes; dropping {}",
            entries[last]
        );
        entries.remove(last);
    }
    entries.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_shells_entries_come_first_and_the_defaults_stay() {
        let path = merge_guest_path(
            "/root/.local/share/mise/installs/node/24.13.1/bin:/root/.local/bin:\
             node_modules/.bin:/usr/bin:/root/.local/bin:/bin",
        );
        assert_eq!(
            path,
            "/root/.local/share/mise/installs/node/24.13.1/bin:/root/.local/bin:/usr/bin:/bin:\
             /root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn a_long_login_path_loses_its_last_entries() {
        let entries: Vec<String> = (0..200)
            .map(|index| format!("/opt/tool{index:03}/bin"))
            .collect();
        let path = merge_guest_path(&entries.join(":"));
        assert!(path.len() <= MAX_GUEST_PATH, "{}", path.len());
        assert!(path.starts_with("/opt/tool000/bin:"), "{path}");
        assert!(path.ends_with(DEFAULT_GUEST_PATH), "{path}");
        assert!(!path.contains("/opt/tool199/bin"), "{path}");
    }

    #[test]
    fn the_farm_mirrors_the_login_shells_own_folders() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = Runtime::new(temp.path());
        let rootfs = runtime.rootfs_dir();
        let node = "root/.local/share/mise/installs/node/24.13.1/bin";
        std::fs::create_dir_all(rootfs.join(node)).expect("create node dir");
        std::fs::create_dir_all(rootfs.join("usr/bin")).expect("create usr/bin");
        let guest_path = merge_guest_path(&format!(
            "/{node}:/root/missing/bin:/sdcard/bin:/tmp/bin:/usr/bin"
        ));

        assert_eq!(
            runtime.login_binary_dirs_on(&guest_path),
            [rootfs.join(node)]
        );
    }

    #[test]
    fn the_editor_keeps_the_runtimes_variables() {
        let runtime = Runtime::new(std::path::Path::new("/data/user/0/com.zdroid/files"));
        let environment = [
            (
                "PATH",
                "/root/.local/share/mise/installs/node/24.13.1/bin:/usr/bin",
            ),
            ("HOME", "/root"),
            ("SHELL", "/usr/bin/zsh"),
            ("TMPDIR", "/tmp"),
            ("USER", "root"),
            ("SHLVL", "1"),
            ("ZD_ROOTFS", "/data/data/com.zdroid/files/rootfs"),
            ("ZD_IDENTITY", "0:0"),
            ("JAVA_HOME", "/usr/lib/jvm/java-21-openjdk-arm64"),
            ("__MISE_SESSION", "abc"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()));

        let (guest_path, ops) = runtime.login_shell_changes(environment);
        assert_eq!(
            guest_path.as_deref(),
            Some(
                "/root/.local/share/mise/installs/node/24.13.1/bin:/usr/bin:/root/.cargo/bin:\
                 /root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/sbin:/bin"
            )
        );
        let names: Vec<&str> = ops.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            ["JAVA_HOME", "__MISE_SESSION", engine_env::GUEST_ENV]
        );
        let Some((_, EnvOp::Set(spec))) = ops.last() else {
            panic!("no guest env: {ops:?}");
        };
        assert!(
            spec.to_string_lossy()
                .contains(";PATH=/root/.local/share/mise/installs/node/24.13.1/bin:/usr/bin:"),
            "{spec:?}"
        );
    }
}
