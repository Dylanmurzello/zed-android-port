# Noexec mount on /storage/emulated/0

**Status:** Active (constraint we route around, not a fix per se)
**Phase / Commit:** L3 storage workflow
**Files:** kernel-level constraint; consequences in `crates/gpui_android/src/storage.rs`, `examples/zed_android/src/title_bar.rs`

## Problem

`cargo run` against a project on `/storage/emulated/0/...` fails with:

```
error: could not execute process `target/debug/<bin>` (never executed)
Caused by:
  Permission denied (os error 13)
```

The compile succeeds. The exec fails. File mode is `+x`.

## Constraint

Android FUSE-mounts `/storage/emulated/0` with the `noexec` flag. Verified via
`/proc/mounts` on a Tab S9 Ultra:

```
/dev/fuse /storage/emulated fuse rw,lazytime,nosuid,nodev,noexec,noatime,...
```

The kernel's `noexec` check fires during ELF loading, after path resolution.
**No userland workaround dissolves it on a non-rooted device:**

- Symlinks across the noexec→exec boundary: Android blocks symlink creation
  on `/storage/emulated/0` for regular apps (verified empirically with
  `ln -s` from `run-as`).
- proot bind mounts: don't change kernel-level exec semantics — the check is
  on the *underlying file's* mount, not on the bind path.
- `memfd_create` + `execveat`: blocked by SELinux for `untrusted_app_*` —
  Android sepolicy denies `mmap(PROT_EXEC)` on memfd fds. Confirmed via
  bun-termux-loader README.
- Userland exec (mmap-and-jump): same SELinux block on PROT_EXEC mappings.
- Recompile the kernel: not a userland fix.

The `/mnt/pass_through/0/emulated` underlying f2fs mount **is** exec-allowed,
but inaccessible to regular apps without root.

## Solution

Don't try to defeat the constraint. Reshape the workflow so binaries never
land on the noexec mount in the first place:

- Workspaces live in `~/projects/` (= `$HOME/projects/`, app-private,
  exec-allowed).
- `/storage/emulated/0` is exposed for browse/edit-single-file via Termux-
  style `~/storage/*` symlinks but never as a workspace root.
- SAF picks become a one-shot recursive copy into `~/projects/<basename>` —
  see [projects-workspace-import.md](projects-workspace-import.md).
- If a user opens a project from /sdcard anyway, the title bar shows a
  "Builds won't run · Move" chip with one-tap fix — see
  [noexec-banner-move.md](noexec-banner-move.md).

## Why this works

`/data/data/<pkg>/files` is mounted with default exec permissions. Since
the rootfs engine nothing under `$HOME` is exec'd by the kernel at all: the
loader in the APK's native library dir is exec'd and maps the binary, which
every app domain allows (`app_data_file:file execute`), so the level of
`targetSdk` no longer matters here. A noexec mount refuses that mapping
too (`mmap(PROT_EXEC)` is EPERM there), which is why the banner stays.

## Failure mode if regressed

- A future change that opens a project rooted on `/storage/emulated/0` and
  doesn't trigger the banner = silent EACCES on first build.
- A policy change that drops `app_data_file:file execute` for untrusted
  apps would stop the loader from mapping guest binaries anywhere under
  app data; nothing in-app can route around that (the targetSdk 28 era
  depended on the narrower `execute_no_trans` instead, see
  [targetsdk-28-execve.md](targetsdk-28-execve.md)).

## See also

- [projects-workspace-import.md](projects-workspace-import.md)
- [noexec-banner-move.md](noexec-banner-move.md)
- [deferred-tier2-root-storage.md](deferred-tier2-root-storage.md)
- [targetsdk-28-execve.md](targetsdk-28-execve.md)
