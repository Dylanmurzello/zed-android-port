//! `zd-exec` — generic spawn wrapper. Routes every PATH-resolved
//! invocation (bash, git, rust-analyzer, …) through the configured
//! `RuntimeProvider`.
//!
//! Three invocation shapes:
//!
//!   1. **Symlink** at `$PREFIX/zd-runtime/<name>`. argv[0] basename
//!      *is* the target program name. This is how Zed's `Command::new`
//!      lands here — kernel resolves PATH, finds the symlink, exec's
//!      this binary. We dispatch to `<name>`.
//!
//!   2. **Shell** invocation: `zd-exec` with no positional program, or
//!      with leading `-c`/`-l`-style flags. Happens when alacritty
//!      exec's us as `$SHELL` (`execve(zd-exec, ["zd-exec"], envp)` or
//!      `["zd-exec", "-c", "cmd"]`). We dispatch to `bash` with whatever
//!      flags the caller passed — the integrated terminal lands in the
//!      configured adapter's bash this way.
//!
//!   3. **Direct** target invocation: `zd-exec <program> [args…]`. First
//!      positional is the target binary. Used for testing and one-off
//!      tool invocations from a script.
//!
//! Reads `runtime.toml` from `$PREFIX/etc/zd-runtime.toml` to pick the
//! active adapter, builds a `SpawnRequest` from the current process'
//! cwd / env / stdio, calls `provider.spawn(req).wait()`, and exits
//! with the child's exit code (or `128 + signum` if killed by signal,
//! matching bash semantics).
//!
//! No `su` fallback. If the chroot adapter can't reach `zd-spawnd`,
//! we fail loudly with a hint — silently re-execing through `su` is
//! how the per-spawn fork-bomb regression sneaks back in (see memory:
//! `project_runtime_swap_architecture`).

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zdroid_runtime::adapters;
use zdroid_runtime::config::RuntimeFile;
use zdroid_runtime::port::SpawnRequest;

/// Same path the picker writes to. Hardcoded — the wrapper has no way
/// to discover `$PREFIX` other than by reading the env, and we want
/// the wrapper's behavior to be deterministic regardless of who
/// invoked it (Zed, an interactive shell, a daemon).
const RUNTIME_TOML: &str = "/data/data/com.zdroid/files/usr/etc/zd-runtime.toml";

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().collect();
    let argv0 = argv.first().cloned().unwrap_or_default();
    let basename = Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&argv0);

    let (program, prog_args): (String, Vec<OsString>) = if basename == "zd-exec" {
        // Shell-mode dispatch: no args, or argv[1] is a flag (alacritty
        // commonly invokes `$SHELL -c '<cmd>'` for non-interactive
        // task spawns). Forward as bash <flags>.
        match argv.get(1) {
            None => {
                // Login shell so the chrooted bash sources /etc/profile
                // and ~/.profile in addition to ~/.bashrc — debian /
                // kali put `~/.local/bin` and similar on PATH from
                // ~/.profile, which a non-login interactive shell
                // wouldn't pick up. Without -l: claude (and any other
                // user-installed tools) silently disappear from PATH.
                ("bash".to_string(), vec![OsString::from("-l")])
            }
            Some(first) if first.starts_with('-') => {
                let rest = argv.iter().skip(1).map(OsString::from).collect();
                ("bash".to_string(), rest)
            }
            Some(prog) => {
                let rest = argv.iter().skip(2).map(OsString::from).collect();
                (prog.clone(), rest)
            }
        }
    } else {
        let rest = argv.iter().skip(1).map(OsString::from).collect();
        (basename.to_string(), rest)
    };

    let provider = match build_provider() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("zd-exec: {e:#}");
            return ExitCode::from(127);
        }
    };

    let cwd = env::current_dir().ok();
    let env_map: HashMap<String, OsString> = env::vars_os()
        .filter_map(|(k, v)| k.into_string().ok().map(|k| (k, v)))
        .collect();

    let req = SpawnRequest {
        program,
        args: prog_args,
        cwd,
        env: env_map,
        interactive: std::io::stdin().is_terminal(),
        stdio: [0, 1, 2],
    };

    let mut handle = match provider.spawn(req) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("zd-exec: spawn: {e:#}");
            return ExitCode::from(127);
        }
    };

    match handle.wait() {
        Ok(code) if code >= 0 => ExitCode::from(code.min(255) as u8),
        Ok(code) => {
            let signum = (-code).clamp(0, 127) as u8;
            ExitCode::from(128 + signum)
        }
        Err(e) => {
            eprintln!("zd-exec: wait: {e:#}");
            ExitCode::from(127)
        }
    }
}

fn build_provider() -> anyhow::Result<Box<dyn zdroid_runtime::port::RuntimeProvider>> {
    let path = PathBuf::from(RUNTIME_TOML);
    let file = RuntimeFile::load(&path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{} not found. Open Zdroid and pick a runtime in Settings first.",
            path.display(),
        )
    })?;
    let resolved = file.resolve()?;
    adapters::for_config(&resolved)
}
