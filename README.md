<p align="center">
  <img src="crates/gpui_android/examples/zed_android/docs/screenshots/zdroid-logo.png" width="120" alt="Zdroid logo" />
</p>

<h1 align="center">Zdroid</h1>

<p align="center"><sub><em>Zed on Android.</em></sub></p>

<p align="center">
  Started as a joke. Rust on aarch64, sounded portable. Laughed about it. Kept going. Couldn't stop. There's an APK.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/status-alpha-orange" alt="Alpha" />
  <img src="https://img.shields.io/badge/platform-Android-3DDC84?logo=android" alt="Android" />
  <a href="https://github.com/Dylanmurzello/zed-android-port/releases/latest"><img src="https://img.shields.io/github/downloads/Dylanmurzello/zed-android-port/total?label=downloads" alt="Total downloads" /></a>
</p>

Zdroid is an independent port of [Zed](https://zed.dev) for Android, not affiliated with Zed Industries. Upstream's `Editor`, `Workspace`, `Project`, `Search`, `GitGraph`, `Extensions`, and `Terminal` crates run unchanged on a custom `gpui_android` platform backend that composites every pixel via the Adreno Vulkan driver, targeting Android 9+ with a hardware keyboard. A bundled Linux userland (Termux-derived, repackaged under `com.zdroid`) lets apt, bash, git, ssh, node, go, and rust-analyzer all run in-process from the app's private data dir; alternative runtime adapters route through a Kali chroot or an existing Termux install.

---

<p align="center">
  <img src="https://github.com/user-attachments/assets/68f763ba-e051-4217-8779-7bb9327f4a13" alt="Zdroid demo" width="100%" />
</p>

Vulkan via wgpu. AChoreographer-driven vsync, no JNI hop per frame. Opt-in 120Hz with Mailbox present mode. Glyph fallback into `/system/fonts` so Powerline arrows and CJK render without bundling fonts. The `Editor`, `Workspace`, `Project`, `MultiWorkspace`, `Search`, `GitPanel`, `GitGraph`, `Extensions`, and `Terminal` crates run unchanged. The Rust `.so` is the app process. gpui composites every pixel (yes, you read that right) straight into the Adreno Vulkan driver. Multi-Activity OS-chromed extra windows so DeX freeform renders Settings and secondary editors with real chrome.

Termux userland rebuilt under `com.zdroid` (applicationId byte-length pinned to 10 because prebuilt RUNPATHs in the .debs don't stretch). Musl loader hex-patched at runtime so Bun-compiled binaries like claude-code and codex resolve `/etc/resolv.conf` to a JNI-populated `/sdcard/.zed/r`. Optional Magisk-flashable `zd-spawnd` daemon with SCM_RIGHTS stdio relay for the chroot runtime. SurfaceControl-composited hardware cursor sprite on a sibling overlay, separate from the wgpu frame. Pointer-capture trackpad that consumes historical motion samples so finger drags don't lose 80% of their travel to event batching. SAF DocumentsProvider exposing `~/` as a system volume. Native Android trust via `rustls-platform-verifier`. In-app updater pulling signed APKs from GitHub Releases. Everything else is upstream. Deep-dives for the platform layer live in [`crates/gpui_android/docs/workarounds/`](crates/gpui_android/docs/workarounds/).

---

## <img src="https://api.iconify.design/lucide:download.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Install

Grab the latest `Zdroid-X.Y.Z.apk` from the [releases page](https://github.com/Dylanmurzello/zed-android-port/releases/latest) and open it in your file manager. Android prompts for unknown-source installs the first time; grant it. Reinstalls upgrade in place because every release ships from the same signing cert.

> [!NOTE]
> Android may show a "built for an older version of Android" warning before you tap Install. Proceed anyway. `targetSdk` is pinned at 28 on purpose: the bundled Termux userland depends on the `untrusted_app_27` SELinux domain, which permits `execve` on app-private files. Bumping `targetSdk` to 29+ lands the process in a stricter domain that denies exec, and the entire runtime stops working. See [`docs/workarounds/targetsdk-28-execve.md`](crates/gpui_android/docs/workarounds/targetsdk-28-execve.md) for the receipts.

### First launch

1. **Storage permissions.** The system prompts for read/write to `/sdcard` so the editor can reach anything outside its app-private dir. Grant it. Without it, "Open Project" can't see your files.
2. **Runtime adapter.** A picker asks where every subprocess (shells, LSPs, terminal, git, ssh) should run. Nothing is pre-selected; you pick one of three. **Bootstrap** is the no-root option: a Termux-derived userland that runs entirely from the app's data dir, the right choice for most people. **Kali chroot** needs Magisk plus a Kali NetHunter rootfs but gives you real glibc and the fastest spawn. **External Termux** routes through your existing Termux app if you already daily-drive Termux.
3. **Bootstrap download.** If you pick Bootstrap, the adapter pulls the userland zip from [`Dylanmurzello/zdroid-bootstrap`](https://github.com/Dylanmurzello/zdroid-bootstrap) and extracts it into the app's private data dir. About 30 seconds on a fast connection. Subsequent launches are instant.

### Setting up your shell environment (Bootstrap)

Open the integrated terminal. First, sync the package index:

```sh
pkg update && pkg upgrade
```

Pre-baked in the bootstrap: rust-analyzer (the LSP binary; `cargo` and `rustc` are not bundled, install with `pkg install rust` if you want them), nodejs, go, bash, openssh, busybox, the bionic-compat patchelf, the hex-patched musl loader. `npm` is a separate Termux package and needs an install before user-facing `npm` calls work. Claude Code, for example:

```sh
pkg install npm
npm install -g @anthropic-ai/claude-code
```

Toolchains and LSPs for other languages have install recipes in the bootstrap repo: [`Dylanmurzello/zdroid-bootstrap`](https://github.com/Dylanmurzello/zdroid-bootstrap).

> [!NOTE]
> The first `pkg install` after extracting a fresh bootstrap will surface "broken dependencies" from apt and prompt you to run `apt --fix-broken install`. Run it. The pre-baked packages (`go`, `openssh`, busybox, etc.) declare dpkg dependencies that aren't formally registered in the database on a fresh extract, so apt flags the inconsistency the first time it has to resolve anything. fix-broken reconciles the state; `pkg` works normally afterward.

### Setting up Kali chroot

Prerequisites: a rooted device with Magisk installed.

1. Drop a Kali NetHunter aarch64 rootfs at `/data/local/nhsystem/kali-arm64`. NetHunter's installer is the standard path; any aarch64 Debian-derived rootfs at that location works.
2. Flash the [`zd-spawnd`](https://github.com/Dylanmurzello/zdroid-spawnd/releases) Magisk module. Reboot.
3. Open Zdroid and pick **Kali chroot** in the runtime picker.

Every subprocess the editor spawns (`bash`, `git`, LSPs, terminal shells) goes over a Unix socket to the `zd-spawnd` daemon, which `fork`s, `chroot`s, drops privileges, and `execve`s on your behalf. Sub-millisecond spawn versus ~200 ms for `su`-mediated alternatives. All the bionic-vs-glibc gotchas (`/usr/bin/env`, `/tmp`, `dlopen libfoo.so`) disappear because subprocesses run inside a real distro.

### Setting up External Termux

> [!IMPORTANT]
> External Termux is partially wired today. The runtime-picker entry exists and persistence works, but the JNI Intent bridge that actually dispatches subprocesses to Termux's `RUN_COMMAND` service hasn't landed yet ([`crates/zdroid_runtime/src/adapters/external_termux.rs`](crates/zdroid_runtime/src/adapters/external_termux.rs) is stubbed). Picking this adapter today writes the selection, but subprocess calls don't reach Termux. Use Bootstrap or Kali chroot for actual work in the meantime.

Setup, once the bridge lands:

1. Install Termux from [GitHub releases](https://github.com/termux/termux-app/releases).
2. The first time Zdroid attempts an external spawn, Android prompts you to grant `com.termux.permission.RUN_COMMAND` to Zdroid. Allow it.
3. Open Zdroid and pick **External Termux** in the runtime picker.

After that, every subprocess routes into your existing Termux setup via `com.termux.app.RunCommandService`. Your `~/`, your packages, your shell history are what subprocesses see; Zdroid stays a thin spawner.

### Working with projects

Two storage realms underneath, with different exec rules.

`/data/data/com.zdroid/files/` (surfaced as `~/`) is **exec-mounted**. cargo, go, node, anything you build can `execve` and run. This is where projects should live. `~/projects/<name>` is the default workspace root; `ZedDocumentsProvider` exposes `~/` to other Android apps via the SAF sidebar (look for **Zdroid** in any system file picker).

`/storage/emulated/0/` (a.k.a. `/sdcard/`) is **FUSE-mounted with `noexec`**. Read, edit, and save all work; the kernel refuses to execute binaries written here. `cargo run` against a binary under `/sdcard/...` returns `EACCES` and there's no remount workaround (see [`docs/workarounds/android-noexec-mount.md`](crates/gpui_android/docs/workarounds/android-noexec-mount.md) for why).

Three workflows that work with this constraint:

- **Project root under `~/projects/`** is the happy path. `git clone`, `cargo new`, builds, debugs, terminal subprocesses all run. Browse there from any Android app via the **Zdroid → projects** SAF sidebar entry.
- **Open a folder anywhere on `/sdcard/`** is fine if you're only reading or editing. The title bar shows a yellow **Builds won't run · Move** chip; one tap pops a confirm dialog that copies the project into `~/projects/<basename>` and reopens it from the exec side.
- **`~/storage/{shared,downloads,dcim,documents,…}`** are curated symlinks into `/sdcard`. Use them for "open and edit a single file" workflows where you don't want to copy a whole tree.

---

## <img src="https://api.iconify.design/lucide:hand.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Gestures & input

Each surface toggles in **Settings → Android Input** (or on the first-run onboarding card).

### Touch (default)
- **Tap** to position the cursor
- **Tap and drag** to scroll
- **Two-finger tap** for right-click
- **Long-press** to select a word
- **Long-press and drag** to extend the selection
- **Scrollbar tap and drag** snaps the thumb to your finger and follows

### Virtual trackpad mode
Toggle the crosshair icon in the tab bar to turn touch into a pointer. Direct touch is disabled while it's on.
- **One-finger drag** moves the cursor
- **Tap** for left click, **two-finger tap** for right click
- **Hold then drag** for text selection
- **Two-finger drag** to scroll the content

### Hardware mouse / trackpad
Plug in or pair a mouse, trackpad, or Book Cover Keyboard and it just works.
- **Scroll wheel** and **two-finger trackpad scroll** to scroll
- **Right-click** anywhere for context menus
- The cursor hides when you switch to touch or typing and comes back on the next pointer event

> Working on Samsung tablets and most Android devices we've tested. If your mouse pairs but doesn't move the cursor (seen on some OnePlus / Oppo Pad ColorOS builds), [open an issue](https://github.com/Dylanmurzello/zed-android-port/issues/new) with `adb shell dumpsys input` output so we can triage.

### Soft keyboard
Auto-opens on tap into the editor or terminal. The pane tab bar has a keyboard toggle if you want it off.
- **Programming keys row** above the keyboard with `Esc`, `Tab`, `Ctrl`, `Alt`, and arrows. `Ctrl` and `Alt` are sticky: tap once for the next key, double-tap to lock.
- **`Ctrl` + letter** combos work in the terminal: `Ctrl+C` sends `^C`, just like a hardware keyboard.

### Multi-window
Settings, the runtime picker, and any other spawned window each open as their own Recents entries and get their own soft keyboard and input.

---

<a id="userland"></a>
## <img src="https://api.iconify.design/lucide:server.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Userland

The editor is bionic-linked and runs as the Android app process. Every subprocess it spawns (`bash`, `apt`, language servers, formatters, terminal shells, `git`, `ssh`) routes through whichever runtime adapter the user picked. Three adapters ship; they version independently of the editor APK.

| Adapter | What it is | Where it comes from |
|---|---|---|
| **Bootstrap** _(no root)_ | Termux userland rebuilt under `com.zdroid`: apt/dpkg/bash with our package name baked into RUNPATHs and shebangs. Pure bionic, no glibc; same trade-offs as any Termux install. `apt` and `pkg install` work for everything Termux ships. | Downloaded from [`Dylanmurzello/zdroid-bootstrap`](https://github.com/Dylanmurzello/zdroid-bootstrap) after you pick Bootstrap in the runtime picker. |
| **Kali chroot** _(needs Magisk)_ | Real glibc Linux. Every spawn goes over a Unix socket to `zd-spawnd` (a small privileged daemon) which does `fork` + `chroot` + `setuid` + `execve` on the editor's behalf. ~5 ms per spawn vs ~200 ms for `su`-mediated. All the bionic gotchas (`/usr/bin/env`, `/tmp`, `dlopen libfoo.so`) disappear because subprocesses run inside a real distro. | Flash the Magisk module from [`Dylanmurzello/zdroid-spawnd`](https://github.com/Dylanmurzello/zdroid-spawnd), plus drop a Kali NetHunter aarch64 rootfs at `/data/local/nhsystem/kali-arm64`. |
| **External Termux** _(if you already use Termux)_ | Talks to your existing Termux app via `com.termux.permission.RUN_COMMAND` intents. Lighter footprint; your existing userland stays untouched. JNI Intent bridge in progress (adapter at [`crates/zdroid_runtime/src/adapters/external_termux.rs`](crates/zdroid_runtime/src/adapters/external_termux.rs) is stubbed). | Install Termux from [GitHub releases](https://github.com/termux/termux-app/releases); grant `RUN_COMMAND` to Zdroid. |

Switching is one tap (Settings → Android Runtime). Selection persists in `$PREFIX/etc/zd-runtime.toml`. **Restart Zdroid after switching adapters.** The editor caches environment state (PATH, HOME, library search paths, spawn-router config) from whichever adapter was active at boot; without a restart, subprocesses spawned post-switch can land with stale env and fail in cryptic ways (LSPs not found, `git` claiming HOME doesn't exist, `pkg install` writing to the wrong rootfs, etc.).

### When to pick which

- **No root, just want it to work:** Bootstrap. apt, npm, `go install`, rust-analyzer all work. Rough edges: precompiled Bun CLIs (claude-code, codex) rely on a runtime hex-patch of `/etc/resolv.conf` to a `/sdcard/.zed/r` file that JNI populates each boot from `ConnectivityManager.getActiveDnsServers()`. Full writeup in [`zdroid-bootstrap/docs/hex-patch-resolv-conf.md`](https://github.com/Dylanmurzello/zdroid-bootstrap/blob/main/docs/hex-patch-resolv-conf.md). Some glibc-only extension binaries don't run.
- **Have Magisk, want real Linux:** Kali chroot. Everything you'd expect on Debian/Kali works as-is, no shimming needed. The chroot is shared with whatever else uses that NetHunter rootfs.
- **Already on Termux:** External adapter once the bridge lands. Your `~/`, your packages, your shell history; Zdroid just spawns subprocesses there.

---

## <img src="https://api.iconify.design/lucide:layers.svg?color=%23999999&height=22" valign="middle" /> &nbsp;What works

<p align="center">
  <table>
    <tr>
      <td><img src="crates/gpui_android/examples/zed_android/docs/screenshots/ssh_workspace.jpg" alt="Remote SSH workspace" /></td>
      <td><img src="crates/gpui_android/examples/zed_android/docs/screenshots/git_graph.jpg" alt="Git graph" /></td>
      <td><img src="crates/gpui_android/examples/zed_android/docs/screenshots/settings.jpg" alt="Settings" /></td>
    </tr>
  </table>
</p>

- **Editor.** Vulkan rendering, multi-pane workspace, vim mode, syntax highlighting, project panel, fuzzy file finder, command palette, buffer + project search.
- **Git.** Git panel with staging, full git-graph commit history, diff view.
- **LSPs.** rust-analyzer baked in. gopls, ts-server, pyright, jdtls install in one `pkg`/`npm`/`go install`.
- **Extensions.** Browse, install, manage. Themes, language configs, grammars, slash commands.
- **Remote SSH.** Server-picker pill in the title bar, persisted server list, native askpass.
- **Multi-window.** Android freeform and DeX, each extra window is a real Activity with OS chrome.
- **Edge-to-edge** rendering with content under the display cutout.
- **App menu bar** with nested submenus (Settings, Keymap, Themes, Extensions).
- **Theme follow** for system light/dark.
- **`ZedDocumentsProvider`** exposes the project root as a SAF volume, so other Android apps can browse Zed's worktrees.

---

## <img src="https://api.iconify.design/lucide:hammer.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Build from source

You'll need:

- Rust toolchain with `aarch64-linux-android` (`rustup target add aarch64-linux-android`)
- [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk) (`cargo install cargo-ndk`)
- Android NDK r27 (`sdkmanager "ndk;27.0.12077973"`)
- Gradle 8+, `adb` on `$PATH`
- A device with USB debugging on

```sh
cd crates/gpui_android/examples/zed_android

ANDROID_NDK_HOME=/path/to/ndk/27.0.12077973 \
  cargo ndk -t arm64-v8a -P 26 -o android/app/src/main/jniLibs build

cd android
gradle assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.zdroid/.MainActivity

adb logcat -d | grep -E "zed_android|RustPanic|FATAL"
```

First build is around 10 minutes. Incremental Rust rebuilds are 20 seconds, Gradle re-pack a few seconds.

---

## <img src="https://api.iconify.design/lucide:tablet-smartphone.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Tested on

Samsung Galaxy Tab S9 Ultra (Snapdragon 8 Gen 2 / Adreno 740, Android 16, One UI 8) is the daily driver. Compiles for any aarch64 Android 9+ with Vulkan 1.1, but only Adreno is exercised. Mali / Xclipse will run but may want shader tweaks.

A hardware keyboard is the supported config. Tablet plus Bluetooth keyboard, foldable in tablet mode, or DeX/desktop-mode with monitor and peripherals all work. Phones technically run but are de-prioritized; see [`docs/workarounds/deferred-phone-form-factor-polish.md`](crates/gpui_android/docs/workarounds/deferred-phone-form-factor-polish.md).

---

## <img src="https://api.iconify.design/lucide:file-text.svg?color=%23999999&height=22" valign="middle" /> &nbsp;License

GPL-3.0-or-later, same as upstream Zed. The Bootstrap-adapter zip (distributed from [`Dylanmurzello/zdroid-bootstrap`](https://github.com/Dylanmurzello/zdroid-bootstrap), not bundled in the APK) contains Termux-rebuilt packages each under its own license (mostly BSD/MIT/Apache; gnupg/bash/coreutils are GPL). The Alpine-derived `ld-musl-aarch64.so.1` inside it is MIT. The `zd-spawnd` daemon ([`Dylanmurzello/zdroid-spawnd`](https://github.com/Dylanmurzello/zdroid-spawnd)) is GPL-3.0-or-later.

© Dylan Murzello, distributed under GPL-3.0-or-later. Zed itself is © Zed Industries.

---

## <img src="https://api.iconify.design/lucide:handshake.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Acknowledgments

- [Zed Industries](https://zed.dev/) for [`gpui`](https://github.com/zed-industries/zed/tree/main/crates/gpui) being platform-agnostic enough that an Android port is plumbing rather than a rewrite.
- The [`wgpu`](https://github.com/gfx-rs/wgpu) and [`blade-graphics`](https://github.com/kvark/blade) maintainers for a Vulkan abstraction that just works on Adreno.
- [The Termux project](https://termux.dev/) for [a decade of Linux-on-Android](https://github.com/termux/termux-app). Most of our `apt install` machinery is their patches with the package name swapped.
- [Alpine Linux](https://alpinelinux.org/) for [musl libc](https://musl.libc.org/), which lets Bun-compiled musl binaries (claude-code, codex) execve cleanly on bionic.

---

## <img src="https://api.iconify.design/lucide:circle-help.svg?color=%23999999&height=22" valign="middle" /> &nbsp;So why this ?

Zed Industries' position on a mobile/tablet port: **not planned**.

- [#12039 IOS/Android Port](https://github.com/zed-industries/zed/issues/12039), open since May 2024.
- [#34633 start of termux build](https://github.com/zed-industries/zed/issues/34633), closed as "not planned" in Jul 2025.
- [#43207 gpui: On Android](https://github.com/zed-industries/zed/issues/43207), open in the GPUI Roadmap as "Wide Scope" since Nov 2025.

This repo is what those threads were asking for, built independently. The Termux build attempt failed because the upstream `wasmtime`/`cranelift` deps don't compile inside Termux. We sidestep that by building the APK on a desktop with `cargo-ndk` and running our own custom Termux userland in process. No fork of upstream-Zed-with-android-cfg is needed; the Editor, Workspace, Project, Search, GitGraph, Terminal, Extensions crates run unchanged. The work is at the platform boundary, documented in [`crates/gpui_android/docs/workarounds/`](crates/gpui_android/docs/workarounds/).
