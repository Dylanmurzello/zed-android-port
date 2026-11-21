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
  symlink rewritten relative (links the guest makes later store an
  absolute target as its host path instead, so they survive a rename into
  another folder), modes kept, device nodes skipped, seeds for
  `resolv.conf`, `hostname`, `hosts`, the device's time zone (`/etc/localtime` as a relative link into
  `zoneinfo` plus `/etc/timezone`, from `persist.sys.timezone`: the base
  image has no tzdata, and without the link the first `apt install` that
  pulls it in stops at an interactive time zone menu), the debugger
  configs and the binding directory chains, then a two-rename swap. Seconds on a tablet, no network. The essentials step (`apt-get
  install ca-certificates git openssh-client tzdata`, which the base image lacks) runs in the
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
  editor's children (`runtime_service::terminate_descendants`), extracts
  the bundled image again over a fresh staging dir and carries the old
  tree's `/root` into the new one; then the essentials step the first
  launch runs (git, the SSH client, CA certificates and time zone data)
  follows, since the base image has none of them. The app's home folder
  and the editor state are outside the rootfs and untouched.
- **The home folder.** The guest's home is the image's `/root`, inside
  the rootfs: `$HOME` in the guest view and root's entry in the rootfs's
  `/etc/passwd`, which ssh reads instead of `$HOME`. The editor's `HOME`
  is the same directory in the host's spelling
  (`<data>/files/rootfs/root`), so `~` is one folder to both. Dotfiles,
  SSH keys and per-user toolchains (`~/.cargo`, `~/.local`) live there;
  `~/projects`, `~/storage` and `~/received` are links
  (`install::link_home`) into the app's home folder `<data>/files/home`,
  which the Files app shows as Zdroid's root and Zed's picker opens.
  Projects belong under `~/projects`: the files dir is identity-mapped,
  so a path there reads the same on both sides, while a tool's `getcwd`
  elsewhere under `/root` answers in the guest spelling (`/root/...`),
  which the editor does not map back. A reset carries `/root` into the
  new tree. `rootfs-guest-env.md` has the reasoning.
- **Upgrades from Bootstrap and chroot releases.** They aren't migrated:
  their programs, and what Zed installed with them, don't run in the
  rootfs. Boot removes the old prefix (`<data>/files/usr`), the
  `bin/zd-exec` bridge and `etc/zd-runtime.toml`
  (`runtime_setup::sweep_previous_runtimes`) and sets the rootfs up as on
  a first launch; the home folder and editor settings stay. Their home was
  the app's home folder, so `link_home` also links its `.ssh`, `.claude`,
  `.claude.json` and `.codex` into `/root` and includes its `.gitconfig`
  from root's.

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
