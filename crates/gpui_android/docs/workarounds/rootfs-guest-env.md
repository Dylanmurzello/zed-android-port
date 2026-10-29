# The guest environment contract

**Status:** Active
**Phase / Commit:** rootfs engine, chunk 2
**Files:** `crates/zdroid_runtime/src/{runtime,env}.rs`, `crates/gpui_util/src/lib.rs` (`guest_env_for_path`), `crates/util/src/command.rs` (`env`), `crates/terminal/src/terminal.rs` (Android env block), `crates/remote/src/transport/ssh.rs` (strip list), `crates/project/src/environment.rs` and `crates/languages/src/{rust,python}.rs` (comments)

## Problem

Two processes with different ideas of the world share one environment.
The editor is a bionic process whose `HOME` is the app files dir and
whose PATH has to find the farm; the tools it spawns are Ubuntu
processes that expect `HOME=/root`, `PATH=/usr/local/sbin:...` and
`TMPDIR=/tmp`. Upstream Zed hands `std::env::vars()` to every worktree
and language server, so whatever the editor's environment is, the tools
inherit it.

## Constraint

The rewrite has to happen where the two worlds meet, at exec, and only
for the spawns the editor initiates. A guest shell that does `export
PATH=...`, `su -` or `env -i` must see its own change honoured by every
child it starts, so the engine must not re-apply the editor's view at
every exec.

## Solution

- **Editor process (`Runtime::env_for_zed_process`).** `baseline_env`
  keeps the host shape: `HOME=<data>/files`, `TMPDIR=<data>/files/tmp`,
  `PATH=<data>/files/zd-runtime:<data>/files/bin:<system PATH>`, `TERM`,
  `COLORTERM`, `LANG`, `ZED_BUILD_REMOTE_SERVER=never`, no `LD_PRELOAD`.
  On top of it the engine block every loader invocation needs:
  `ZD_ROOTFS`, `ZD_BINDS`, `ZD_FAKE_ROOT=1`, `ZD_HOSTNAME=zdroid`,
  `ZD_CLOSE_FDS=1`, `ZD_GUEST_ENV`, and `ZD_LOG` when the dev toggle file
  `<data>/files/etc/zd-engine-log` exists. `SHELL` is the loader link.
- **Guest view (`ZD_GUEST_ENV`).** `HOME=/root;USER=root;LOGNAME=root;
  SHELL=/bin/bash;PATH=<guest PATH>;LANG=C.UTF-8;TMPDIR=/tmp`. The loader
  applies it at an exec the editor initiates and removes it from the
  guest's environment, and the handler never re-injects it, so a guest
  process changing its own environment is honoured downstream. The
  persistent `ZD_ROOTFS`, `ZD_BINDS`, `ZD_FAKE_ROOT`, `ZD_HOSTNAME` and
  `ZD_LOG` are re-injected into children (they are engine configuration,
  not the guest's business).
- **A PATH set on one spawn (`gpui_util::guest_env_for_path`).** npm runs
  lifecycle scripts as `sh -c "node install.js"` with the managed Node's
  `bin` prepended to PATH by Zed. The guest PATH from `ZD_GUEST_ENV`
  would replace it, so `util::command::Command::env("PATH", ...)` rewrites
  `ZD_GUEST_ENV` for that spawn: entries that exist in the guest view keep
  their place ahead of the guest PATH, Android partitions and the farm
  (which points at the loader itself) are dropped.
- **Identity-mapped files dir.** `ZD_BINDS` maps `/data/data/com.zdroid/
  files` and `/data/user/0/com.zdroid/files` to the files dir, so a
  project path means the same thing to the editor and to a tool; `/tmp`
  and `/dev/shm` are bound to `<data>/files/tmp`, `/sdcard` and `/storage`
  pass through. The host side of every binding and `ZD_ROOTFS` use the
  kernel's spelling of the files dir (`Runtime::kernel_data_path`), and
  the `/data/user/0` alias is listed first so a kernel path maps back to
  the spelling Zed passed: `android-files-dir-spelling.md`.
- **Terminal.** alacritty replaces the inherited environment with its
  explicit map, so the Android branch of `terminal.rs` copies `HOME`,
  `PATH`, `SHELL`, `TMPDIR` and `LANG` from the process and applies the
  runtime's overlay (empty: the engine block is already in the process
  env). The shell is `zdload -l`, which the loader runs as `/bin/bash -l`.
- **SSH remotes.** The transport strips `LD_PRELOAD`, `HOME`, `TMPDIR` and
  every `ZD_*` from the environment it captures for the remote server, so
  the tablet's view never reaches a host (verified against a Mac: the
  server processes carry none of them).

## Why not the obvious fix

Making `HOME=/root` for the editor too would break every upstream
`dirs::home_dir()` consumer (settings, databases, logs all derive from
it). Applying `ZD_GUEST_ENV` at every exec was the first version and
silently undid `export PATH`, `su -` and `env -i` inside the guest.

## Failure mode

A tool that cannot find another tool the user installed: check the farm
(`ls <data>/files/zd-runtime`) and the guest PATH in `ZD_GUEST_ENV`. A
remote server complaining about `ZD_*` or a preload: the strip list in
`ssh.rs` lost an entry in a merge.

## See also

- `rootfs-engine-integration.md`
- `remote-development-on-android.md`
