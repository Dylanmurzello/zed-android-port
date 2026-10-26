//! Environment the Zed process runs with, shared by every adapter.

use std::ffi::OsString;
use std::path::Path;

use util::env::EnvOp;

/// Env for the Zed process when no adapter is active, and the base the
/// adapters extend. Keeps the process bionic-clean and puts the
/// zd-runtime symlink farm first on PATH so kernel PATH lookups from
/// `Command::new("java")` resolve through the bridge.
pub fn baseline_env(data_path: &Path) -> Vec<(String, EnvOp)> {
    let zd_runtime = data_path.join("zd-runtime");
    let bin = data_path.join("bin");
    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let mut path = OsString::new();
    path.push(&zd_runtime);
    path.push(":");
    path.push(&bin);
    path.push(":");
    path.push(&existing_path);

    vec![
        // HOME stays the app's data root for upstream `dirs::home_dir()`
        // consumers; adapters map the same path into their guest.
        ("HOME".into(), EnvOp::Set(data_path.as_os_str().to_owned())),
        ("TMPDIR".into(), EnvOp::Set(data_path.join("tmp").into_os_string())),
        ("TERM".into(), EnvOp::Set(OsString::from("xterm-256color"))),
        ("COLORTERM".into(), EnvOp::Set(OsString::from("truecolor"))),
        ("LANG".into(), EnvOp::Set(OsString::from("en_US.UTF-8"))),
        // Cross-compiling a remote server on the device is never viable;
        // force upstream's CDN-or-existing-on-remote path.
        ("ZED_BUILD_REMOTE_SERVER".into(), EnvOp::Set(OsString::from("never"))),
        // A leaked preload from an ancestor shell would make every remote
        // SSH subprocess print `cannot be preloaded`.
        ("LD_PRELOAD".into(), EnvOp::Remove),
        ("PATH".into(), EnvOp::Set(path)),
    ]
}
