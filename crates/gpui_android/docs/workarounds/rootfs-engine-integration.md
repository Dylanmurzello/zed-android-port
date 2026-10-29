# Rootfs engine integration

**Status:** Active
**Phase / Commit:** rootfs engine, chunks 0 to 4
**Files:** `crates/zdroid_runtime/src/{runtime,install,env}.rs`, `crates/zdroid_runtime/rootfs-image.properties`, `crates/gpui_android/src/runtime_links.rs`, `crates/zdroid_hooks/src/zdroid_hooks.rs` (`BRIDGE_PROGRAM`, `environment_root_program`), `crates/gpui_android/examples/zed_android/src/{lib,runtime_setup}.rs`, `android/app/build.gradle.kts` (`fetchRootfsImage`, `stageRootfsAsset`), `crates/settings_ui/src/page_data/android.rs` (Android page)

## Problem

Zed spawns tools the way a desktop does: `Command::new("rust-analyzer")`
resolved on PATH, or an absolute path under its data dir for the Node and
language servers it downloads itself. On Android the editor is a bionic
process in an app sandbox, and the tools are Linux binaries that expect
glibc, `/usr`, `/etc` and a root user.

## Constraint

A stock distro binary has `/usr`, `/lib` and `/etc` in its rodata and
its `PT_INTERP`, and no app-writable Android path is that short, so the
rewrite cannot happen in the file. `execve` of app-private files is gated
to targetSdk 28, but `mmap(PROT_EXEC)` of them is allowed at every
targetSdk, and so is `seccomp(2)` with `SECCOMP_RET_TRAP`. The engine
lives on exactly those two facts; the engine itself is documented in the
plan and its crates (`crates/zd_engine_core`, `crates/zd_loader`). This
writeup is the editor side.

## Solution

- **One binary the kernel execs.** `libzdload.so` ships in the APK's
  native library dir (`extract-native-libs-loader.md`). Boot points the
  stable link `<data>/files/bin/zdload` at it (`runtime_links::
  ensure_loader_link`); every persistent reference (`$SHELL`,
  `terminal.shell`, the farm) names the link, since the native library dir
  changes on every install.
- **The rootfs comes with the APK.** `rootfs-image.properties` names the
  Ubuntu base image and its SHA-256; gradle's `fetchRootfsImage` downloads
  and verifies it at build time and `stageRootfsAsset` puts it under
  `assets/rootfs/` (stored, not recompressed). On a launch with no
  `<data>/files/rootfs/.rootfs-version`, `runtime_setup::setup_at_boot`
  streams the asset through the installer before the first paint: every
  symlink rewritten relative (the engine's invariant), modes kept, device
  nodes skipped, seeds for `resolv.conf`, `hostname`, `hosts`, the
  device's time zone (`/etc/localtime` as a relative link into
  `zoneinfo` plus `/etc/timezone`, from `persist.sys.timezone`: the base
  image has no tzdata, and without the link the first `apt install` that
  pulls it in stops at an interactive time zone menu), the debugger
  configs and the binding directory chains, then a two-rename swap. Seconds on a tablet, no network. The essentials step (`apt-get
  install ca-certificates git`, which the base image lacks) runs in the
  background afterwards with a notification; it is the one network-bound
  part and has a button on the settings page for a retry.
- **The farm.** `<data>/files/zd-runtime/<name> -> ../bin/zdload` for
  every program in the rootfs's standard directories (`Runtime::
  list_binaries`), rebuilt at boot and by a watcher on those directories,
  so `apt install` or `cargo install` lands on Zed's PATH within seconds.
  The loader runs the guest program named by `argv[0]`.
- **The bridge.** `util::command::Command::new` and `gpui_util::
  new_std_command` (both through `zdroid_hooks::new_std_command`) rewrite an absolute program under the files dir to
  `zdload <program>`, so a downloaded language server or the managed Node
  runs in the rootfs. Entries under the farm and `bin/` are exempt, else
  the loader would open its own link as the guest program.
- **The environment.** `rootfs-guest-env.md`.
- **Reset.** Settings → Android → Reset Runtime terminates the
  editor's children (`runtime_service::terminate_descendants`) and
  extracts the bundled image again over a fresh staging dir; projects
  and editor state are outside the rootfs and untouched, and the guest's
  home (`/root`) moves into the new tree.
- **The home folder.** `/root` holds links to `~/projects` and
  `~/storage` in the app's home folder (`<data>/files/home`), and to
  what a Bootstrap-era home there held: `.ssh`, `.gitconfig`, `.claude`,
  `.claude.json` and `.codex`. The links point at the identity-mapped
  path, so a tool's `getcwd` under `~/projects` reports the path Zed
  uses. Binding `/root` to the home folder instead would make the
  engine's longest-prefix reverse lookup report `/root/projects/...`,
  which the editor can't match to its worktrees.
- **Upgrades from Bootstrap and chroot releases.** Boot deletes the old bootstrap prefix
  (`<data>/files/usr`), the `bin/zd-exec` binary and the adapter
  selection file; the chroot-era editor state under `home/.zed-env` is
  the user's own settings and is left alone. Farm entries that still
  point at `../bin/zd-exec` are swept by the farm refresh.

## Why not the obvious fix

Downloading the image at first launch was the first version (33 MB from
cdimage.ubuntu.com, SHA256SUMS over TLS, a picker with a progress bar). It
made the app depend on a network for its first terminal and the picker
was a choice with one option. The image is a tenth of the APK, and an
APK that is complete offline needs no install UI at all. Extraction
cannot be skipped: the engine needs real files for the kernel to open and
mmap, an archive cannot be executed in place.

## Failure mode

Setup failing at boot (disk full, a broken APK without the asset) leaves
the editor running without a runtime: a notification carries the reason
and a Retry. The terminal then fails to spawn its shell and language
servers do not start until the retry succeeds. A missing farm entry for a
tool you just installed means the watcher did not fire; the next launch
rebuilds the farm.

## See also

- `rootfs-guest-env.md`, `runtime-foreground-service.md`,
  `android-lsp-auto-restart.md`, `extract-native-libs-loader.md`
- The plan's "Chunk 1 results" through "Chunk 4a findings" for the
  measured engine behaviour
