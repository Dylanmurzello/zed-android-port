# Two spellings of the app files dir

**Status:** Active
**Phase / Commit:** targetSdk 35 bump (2026-10-28)
**Files:** `crates/zdroid_runtime/src/runtime.rs` (`kernel_data_path`, `bind_spec`), `crates/zdroid_hooks/src/zdroid_hooks.rs` (`register_files_dir_spellings`, `respell_kernel_path`, `environment_root_program`), `crates/fs/src/fs.rs` (`RealFs::canonicalize`), `crates/paths/src/paths.rs` (`set_custom_data_dir`), `examples/zed_android/src/lib.rs` (boot)

## Problem

The app files dir has two names on Android: `/data/user/0/com.zdroid/files`
(what `internal_data_path` returns and what the editor keys every stored
path by: recents, workspace trust, settings) and `/data/data/com.zdroid/files`.
The kernel names a file by the route it walked. In the targetSdk 28 era
both routes were mount entries, so `getcwd` and `/proc/*/cwd` reported the
spelling a process had used. At targetSdk 35 on Android 16 the app's
namespace holds one bind mount at `/data/data/<pkg>` and `/data/user/0` is
a symlink to it, so the kernel reports `/data/data/...` for everything,
whatever spelling was used.

Measured after the bump, before the fix:

- Every shell printed `shell-init: error retrieving current directory:
  getcwd: cannot access parent directories: Permission denied` and the
  prompt read `root@zdroid:.#`; Node died in `uv_cwd` with EACCES, which
  killed the TypeScript language server at startup. The engine's getcwd
  returned the kernel path, nothing in `ZD_ROOTFS` or the binding hosts
  (spelled `/data/user/0/...`) matched, the result went back as
  unreachable, and glibc's fallback walked `..` up to `/data/data`, which
  an app may not read.
- `std::fs::canonicalize` turned `/data/user/0/.../projects/hello` into
  `/data/data/.../projects/hello`, so every worktree root changed spelling,
  the trust store (keyed by the old one) called the projects untrusted and
  no language server started, with no prompt on the Android title bar.
  `paths::set_custom_data_dir` canonicalizes too, so `paths::data_dir()`
  would have moved as well, and the spawn bridge's prefix check against
  the editor's root would have stopped bridging the managed Node.

## Solution

One spelling per side, translated at the boundary:

- The engine gets the kernel's. `Runtime::kernel_data_path` is
  `std::fs::canonicalize` of the files dir; `ZD_ROOTFS` and the host side
  of every binding use it, so a path the kernel reports matches. Both
  guest aliases stay bound, `/data/user/0` first, because the engine maps
  a kernel path back to the first binding with that host and `getcwd`
  under a project then prints the spelling Zed passed.
- The editor keeps `/data/user/0`. `set_custom_data_dir` does not
  canonicalize on Android, and `RealFs::canonicalize` replaces the
  kernel's prefix with the editor's (`zdroid_hooks::respell_kernel_path`,
  registered at boot from `Runtime::kernel_data_path`). Worktree roots,
  the trust store and recents keep their spelling across the upgrade.
- The spawn bridge re-spells a program path before its prefix check, for
  a path a tool or the kernel produced.

## Failure mode

`pwd` printing `.` with a getcwd error, or a project opening without
language servers and the log saying `Worktree ... is not trusted` with a
`/data/data/...` path: a path reached the editor in the kernel's spelling
without passing `RealFs::canonicalize`. Direct `std::fs::canonicalize`
calls are the suspects.

## See also

- `rootfs-guest-env.md`: the bindings
- `zdroid_hooks::strip_prefix_by_identity`: inode-based comparison for
  places that compare paths rather than translate them
