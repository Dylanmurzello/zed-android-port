# HOME env dual-pointing

**Status:** Active
**Phase / Commit:** Phase 8 / L3
**Files:** `crates/gpui_android/examples/zed_android/src/lib.rs`, `crates/terminal/src/terminal.rs`

## Problem

Two competing requirements for `$HOME` on Android:

1. **Upstream Zed** calls `dirs::home_dir().expect(...)` in many places
   (`util::paths::home_dir`, `paths::home_dir`, `git::repository`,
   `project::environment`, etc.). On Android, `dirs::home_dir()` returns
   `None` because the JVM sandbox has no system home. Result: panic during
   `Project::local`. Needs `HOME=<some valid dir>`.

2. **Termux convention** is `$HOME = $PREFIX/../home`, i.e. `data_path/home`.
   Termux's profile scripts (.bashrc, /etc/profile) assume this; bash users
   typing `cd ~/projects/...` expect this. If our HOME doesn't match, the
   integrated terminal feels broken.

These two requirements pull in different directions: zed-side needs HOME
*somewhere* writable, Termux side needs HOME at a *specific* spot.

## Constraint

We can't have HOME point at two places simultaneously. The Rust process has
one global env. Subprocesses inherit it.

## Solution

**Dual-pointing**: Rust process keeps `HOME = data_path` (so dirs::home_dir
returns valid), and the integrated terminal subprocess overrides HOME to
`TERMUX__HOME` (= `data_path/home`) on spawn.

In `lib.rs::android_main`:

```rust
unsafe {
    std::env::set_var("HOME", &data_path);
    std::env::set_var("TERMUX__HOME", &termux_home);  // = data_path/home
}
```

In `terminal.rs::insert_android_terminal_env`, the bootstrap runtime's
terminal overlay (registered through `util::env`) sets the terminal's
`HOME` to `TERMUX__HOME`.

The terminal-subprocess env override sets HOME to TERMUX__HOME just for
that subprocess (and its descendants). The Rust globals stay untouched. LSP
spawns inherit Rust's HOME (= data_path) which is fine because rust-
analyzer / gopls / etc. don't care about Termux conventions.

## Why this works

- Rust-side `dirs::home_dir()` returns Some(data_path) → no panic.
- Bash's `$HOME` = data_path/home → `cd ~/projects` resolves to the dir we
  actually create with `setup_user_symlinks` and `mkdir ~/projects`.
- `~/storage/shared` etc. resolve through TERMUX__HOME, hitting our
  curated symlinks correctly.
- LSP children inherit data_path (sufficient for their purposes; they don't
  use ~ in user-visible paths).

## Failure mode if regressed

- If we change Rust HOME to TERMUX__HOME and dirs::home_dir starts returning
  data_path/home, ProjectEnvironment::capture_shell_environment may
  misbehave — it reads HOME to decide where shell rc files live. Currently
  it has special-case shell paths that might assume the data_path layout.
- If we forget the terminal-side override, bash `~/projects` resolves to
  data_path/projects (doesn't exist), and users get "No such file or
  directory" for the directory they obviously just made.

## See also

- [terminal-home-override.md](terminal-home-override.md) — the terminal side
- [projects-workspace-import.md](projects-workspace-import.md) — what we
  put under TERMUX__HOME
