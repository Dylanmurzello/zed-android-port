# Zdroid fork

This repository is a fork of [Dylanmurzello/zed-android-port](https://github.com/Dylanmurzello/zed-android-port), Dylan Murzello's Android port of Zed. It merges each stable [Zed](https://github.com/zed-industries/zed) release, brings more of desktop Zed to Android, and publishes its own signed APKs. This section covers what the fork changes; Dylan's README follows below the line.

### Install

Download `Zdroid-X.Y.Z.apk` from [this fork's releases](https://github.com/ilkersigirci/zed-android-port/releases/latest). After that, Zdroid updates itself from the title bar.

> [!IMPORTANT]
> - This fork's APKs are signed with a different key than Dylan's. To switch, uninstall Dylan's build first. That deletes the app's data, including the runtime and its home folder.
> - Zdroid 0.10 doesn't open the data of 0.9.x and earlier, which ran Bootstrap or a Kali chroot: its first launch asks to clear Zdroid's data. Copy what you want to keep from the home folder first (**Open Files** in that dialog).
> - Release builds are compiled for Armv9-class CPUs: Snapdragon 8 Gen 1, Dimensity 9000, Exynos 2200, Tensor G3 and later. Older CPUs crash at launch, including the Tensor G2 in the Pixel 7 and Pixel Tablet. See [`reference-cpu-build.md`](crates/gpui_android/docs/workarounds/reference-cpu-build.md).

Known issues are listed in each release's notes, from [`.github/release/caveats.md`](.github/release/caveats.md).

### What the fork adds

**Zed, kept current**

- Each Zed stable release is merged in, instead of staying on the Zed version the port started from.
- Desktop Zed's startup code: `keymap.json` and base keymaps, zoom shortcuts, user themes, the unsaved-changes prompt on quit, and settings error notifications.
- The last workspace reopens on launch, SSH remote projects included, following `restore_on_startup`.
- Desktop Zed's editor toolbars and status bar: breadcrumbs, the activity indicator with language server progress and errors, Git stage, unstage and commit buttons in diff views, Open on GitHub and permalinks, image viewer zoom, and diagnostics controls. Git blame can go in the status bar.
- Call hierarchy and the LSP command selector.
- **zed: open log** shows Zdroid's log in the editor, without `adb logcat`. Dialogs use Zed's own style and follow your theme.

**AI**

- The Agent Panel runs ACP agents such as Claude Code and Codex, locally in the Ubuntu runtime and on SSH remotes, and Google Antigravity on SSH remotes. Keep and Reject buttons review their edits.
- Zed's own agent and inline assistant work with your API keys: hosted providers such as Anthropic, OpenAI, Google and OpenRouter, and OpenAI-compatible endpoints ([details](#ai)).
- The screen stays on while an agent works.

**Touch, keyboard and pointer**

- Zed's touch gestures: momentum scrolling, double-tap to select a word, and two-finger swipes that scroll. A finger also drags pane splitters and dock edges. Tabs always show their close button.
- The on-screen keyboard behaves like other Android apps': it shows when you tap a text field or open a search box, not when you open a file, and the window's content stays above it. Its toggle is in the status bar.
- Long press in the terminal opens its context menu, with **Open in Browser** for a selected link.
- Ctrl+click, Shift+click and Alt+click work with a mouse or trackpad, Ctrl+tap opens links, and horizontal wheel scrolling works.
- Samsung DeX: clicks and hover land under the pointer in freeform windows. The Book Cover trackpad follows your finger in landscape, and its two-finger scrolls glide on after the fingers lift, as finger scrolls do.

**Windows**

- Ctrl+W and Ctrl+Shift+W in Settings and other pop-up windows close that window, not all of Zdroid. Closed windows leave Recents.
- Opening Settings no longer freezes for 2 seconds, windows no longer stay blank after the screen turns off, and pop-up windows no longer look washed out while a keyboard or mouse is in use.

**Battery and size**

- Language servers, terminals and SSH sessions keep running in the background while they exist, through a foreground service with an ongoing notification. **Zdroid → Keep Running in Background** keeps the app running without them too, for Zed's own agent, and **Settings → Android → Keep Awake** keeps builds going with the screen off.
- Zdroid draws frames only when something changes and does less work in the background, so it uses much less battery while idle.
- Zed's release size settings, link-time optimization and packed relocations cut about 110 MB from the APK.

**Runtime and SSH**

- Dylan's Ubuntu runtime and engine (his 0.4.0), merged into Zed's current release, with:
  - language servers, debug adapters, Node and ACP agents that Zed downloads picking their Linux builds, the agent registry's included;
  - the SSH client installed on first launch, next to git and the CA certificates;
  - one home folder for the editor, the terminal, the Files app and your projects (`~/projects`), which **Reset Runtime** leaves alone;
  - the terminal runs root's login shell, so `chsh -s /usr/bin/zsh` switches it to zsh, and fonts in `~/.local/share/fonts` or installed with apt reach the editor and the terminal;
  - the runtime's DNS servers following Android's when the network changes;
  - a language server that Android stops restarted, up to three times in ten minutes.
- The debugger, with CodeLLDB.
- SSH remotes download the remote server that matches Zdroid's Zed version, and remote terminals use the remote's own shell.

**Updates, data and security**

- Updates work like desktop Zed's: an **Update Available** button in the title bar, hourly checks (`"auto_update": false` turns them off), download progress, and Android's install session API. An update must be signed with the release certificate.
- Credentials are stored in the Android Keystore. Plaintext ones from older versions are moved there.
- **Open with** in the Files app opens text and source files in Zdroid, where they are.
- **Open Folder** and **Open Files** use Zed's own path picker, which starts in your home folder and reaches `/sdcard` through `~/storage/shared`. `"use_system_path_prompts": true` brings back Android's picker, which lists the home folder as **Zdroid**.
- Cloning into an existing folder or deleting folders from the Files app no longer loses data. Typed text isn't logged, and Android backups are off, since the home folder holds SSH keys and credentials.
- APKs are built and signed in GitHub Actions with a provenance attestation: `gh attestation verify Zdroid-X.Y.Z.apk --repo ilkersigirci/zed-android-port`.

### Contributing

Pull requests go to the `zdroid` branch; `main` mirrors Dylan's repository. [FORK.md](FORK.md) covers syncing Zed and Dylan's repository, and releasing.

---

<p align="center"><sub>Dylan's README for the port follows, updated where Zdroid works differently.</sub></p>

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

Zdroid is an independent port of [Zed](https://zed.dev) for Android, not affiliated with Zed Industries. Zed's editing, workspace, search, Git, extension and terminal components run on a `gpui_android` platform backend with Android-specific adaptations and Vulkan rendering. The reference hardware and supported build configuration are documented in [the platform workarounds](crates/gpui_android/docs/workarounds/README.md). A stock Ubuntu rootfs bundled in the APK runs every tool (apt, bash, git, ssh, node, go, rust-analyzer, gdb) through the app's own userland chroot engine: a static loader and a seccomp trap path translator, no root, no patched packages.

---

<p align="center">
  <img src="https://github.com/user-attachments/assets/68f763ba-e051-4217-8779-7bb9327f4a13" alt="Zdroid demo" width="100%" />
</p>

Vulkan via wgpu. AChoreographer-driven vsync, no JNI hop per frame. Opt-in 120Hz with Mailbox present mode. Glyph fallback into `/system/fonts` so Powerline arrows and CJK render without bundling fonts. Editing, projects, search, Git, extensions, and terminal UI use Zed's native implementations. The Rust `.so` is the app process. gpui composites every pixel (yes, you read that right) straight into the Adreno Vulkan driver. Multi-Activity OS-chromed extra windows so DeX freeform renders Settings and secondary editors with real chrome.

A userland chroot without root or ptrace: the kernel only ever execs a 500 KB static loader shipped in the APK, which maps the Ubuntu binary and glibc's ld.so itself, installs a seccomp filter that traps path syscalls, and rewrites every guest path into the rootfs from an in-thread SIGSYS handler, three microseconds per translated syscall. Static binaries, Go, cargo, strace and lldb all run as on Linux because real ptrace stays free. Same engine answers the syscalls Android's own app filter refuses, which is why stock glibc starts at all. SurfaceControl-composited hardware cursor sprite on a sibling overlay, separate from the wgpu frame. Pointer-capture trackpad that consumes historical motion samples so finger drags don't lose 80% of their travel to event batching. SAF DocumentsProvider exposing `~/` as a system volume. HTTPS trusts the Mozilla root bundle (`webpki-roots`). In-app updater pulling signed APKs from GitHub Releases. Everything else is upstream. Deep-dives for the platform layer live in [`crates/gpui_android/docs/workarounds/`](crates/gpui_android/docs/workarounds/).

---

## <img src="https://api.iconify.design/lucide:download.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Install

Grab the latest `Zdroid-X.Y.Z.apk` from the [releases page](https://github.com/ilkersigirci/zed-android-port/releases/latest) and open it in your file manager. Android prompts for unknown-source installs the first time; grant it. Reinstalls upgrade in place because every release ships from the same signing cert.

### First launch

1. **Access to /sdcard.** Projects under the app's own files need no permission. For anything under `/sdcard` the first launch opens Android's "All files access" toggle for Zdroid (on Android 8 to 10 it is a permission dialog instead); turn it on, or later from **Settings → Android → Allow Access to /sdcard**.
2. **Runtime.** The Ubuntu base image bundled in the APK is extracted into the app's private data dir behind the splash, a few seconds, no network. Then git, the SSH client and the CA certificates are installed with apt in the background; a notification shows progress and needs a network. If that step fails, **Settings → Android → Install Essentials** runs it again.
3. **Notifications and battery.** Language servers and terminals keep running while Zdroid is in the background through a foreground service. Android asks once whether Zdroid may post notifications; declining keeps the service running with its notification hidden. Some skins (One UI's sleeping apps) still stop background apps unless Zdroid is excluded; **Settings → Android → Allow Background Activity** asks for the exemption.

### Setting up your shell environment

Open the integrated terminal. You are `root` in a stock Ubuntu 26.04 (the `ubuntu-base` image: apt, dpkg, bash, perl, no compiler yet). Everything installs the Ubuntu way:

```sh
apt update
apt install -y build-essential curl python3 gdb
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rust-analyzer
```

Zed downloads its own Node for the Node-based language servers (vtsls, eslint, json, tailwind) and for the agent panel's Claude Agent and Codex, so those work without installing Node; `apt install -y nodejs npm` gives you a system one for the terminal. The Claude Code and Codex CLIs, for example:

```sh
apt install -y nodejs npm
npm install -g @anthropic-ai/claude-code @openai/codex
```

In the terminal, run Codex as `codex --no-daemon` (or add `alias codex='codex --no-daemon'` to `~/.bashrc`): by default it starts a background server that sandboxes itself with bubblewrap, which needs namespaces that Android doesn't give an app. The agent panel's Codex doesn't use that server.

Go comes from the official `linux-arm64` tarball unpacked into `/usr/local`; gopls, delve, clangd and the rest install the way their docs say for Linux. Anything that lands in `/usr/bin`, `/usr/local/bin`, `~/.cargo/bin` or `~/.local/bin` shows up on the editor's PATH within a couple of seconds, so language servers start from the editor without configuration.

**Settings → Android → Reset Runtime** extracts the bundled image again when you want a clean slate; your home folder, projects and editor settings stay.

### Working with projects

Two storage realms underneath, with different exec rules.

`/data/data/com.zdroid/files/` is **exec-mounted**. cargo, go, node, anything you build can run there. This is where projects should live: `files/home/projects/<name>`, which the terminal reaches as `~/projects/<name>`, is the default workspace root. `ZedDocumentsProvider` exposes `files/home` to other Android apps via the SAF sidebar (look for **Zdroid** in any system file picker).

`/storage/emulated/0/` (a.k.a. `/sdcard/`) is **FUSE-mounted with `noexec`**. Read, edit, and save all work; the kernel refuses to execute binaries written here. `cargo run` against a binary under `/sdcard/...` returns `EACCES` and there's no remount workaround (see [`docs/workarounds/android-noexec-mount.md`](crates/gpui_android/docs/workarounds/android-noexec-mount.md) for why).

Three workflows that work with this constraint:

- **Project root under `~/projects/`** is the happy path. `git clone`, `cargo new`, builds, debugs, terminal subprocesses all run. Browse there from any Android app via the **Zdroid → projects** SAF sidebar entry.
- **Open a folder anywhere on `/sdcard/`** is fine if you're only reading or editing. The title bar shows a yellow **Builds won't run · Move** chip; one tap pops a confirm dialog that copies the project into `~/projects/<basename>` and reopens it from the exec side.
- **`~/storage/{shared,downloads,dcim,documents,…}`** are curated symlinks into `/sdcard`. Use them for "open and edit a single file" workflows where you don't want to copy a whole tree.

---

## <img src="https://api.iconify.design/lucide:hand.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Gestures & input

Each surface toggles in **Settings → Android** (or on the first-run onboarding card).

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
Auto-opens on tap into the editor or terminal. The status bar has a keyboard toggle if you want it off.
- **Programming keys row** above the keyboard with `Esc`, `Tab`, `Ctrl`, `Alt`, and arrows. `Ctrl` and `Alt` are sticky: tap once for the next key, double-tap to lock.
- **`Ctrl` + letter** combos work in the terminal: `Ctrl+C` sends `^C`, just like a hardware keyboard.

### Multi-window
Settings and any other spawned window each open as their own Recents entries and get their own soft keyboard and input.

---

<a id="userland"></a>
## <img src="https://api.iconify.design/lucide:server.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Userland

The editor is bionic-linked and runs as the Android app process. Every subprocess it spawns (`bash`, `apt`, language servers, debug adapters, formatters, terminal shells, `git`, `ssh`) runs inside a stock Ubuntu 26.04 rootfs extracted from the APK into the app's private data dir. No root, no Magisk, no second app.

How that is possible without root: a distro binary bakes `/usr`, `/lib` and `/etc` into its rodata and its `PT_INTERP`, and no app-writable Android path is that short, so the rewrite cannot happen in the file. It happens at the syscall. The kernel only ever execs `libzdload.so`, a static loader that maps the Ubuntu binary and glibc's own `ld.so`, installs a seccomp filter that traps the path syscalls (`openat`, `execve`, `stat`, `readlink`, `bind` and the rest) and translates the guest path into the rootfs from an in-thread SIGSYS handler, then re-issues the syscall from inside the loader's own text, which the filter lets through. That is proot's idea driven by the kernel's trap instead of a second process and ptrace: three microseconds per translated path syscall instead of two cross-process stops, static binaries run, and real `ptrace` stays free, so gdb, lldb and strace work inside. The same handler answers the syscalls Android's own app filter refuses (`rseq`, `set_robust_list`, the termios2 ioctls, hard links on app data, netlink audit), which is why unmodified glibc, apt and dpkg run at all. Writeups: [`rootfs-engine-integration.md`](crates/gpui_android/docs/workarounds/rootfs-engine-integration.md), [`rootfs-guest-env.md`](crates/gpui_android/docs/workarounds/rootfs-guest-env.md).

Inside, you are `root` (a virtual identity the engine keeps for apt and dpkg; the files belong to the app uid), `$HOME` is the app's home folder (`/data/user/0/com.zdroid/files/home`, the one the Files app shows), which holds your projects and survives a reset, the app's files dir is visible at the same absolute path on both sides so project paths mean the same thing to the editor and to the tools, and `/sdcard` and `/storage` are bound through.

Background survival: a foreground service holds the process out of Android's cached state while a language server or terminal is alive (always, with **Zdroid → Keep Running in Background**), children are reaped when you quit or swipe the task away, and a language server killed from outside is restarted up to three times in ten minutes before it stays stopped with a hint. **Keep Awake** in Settings → Android adds a wake lock so builds continue with the screen off. The device-wide phantom process cap (32 on Android 12+) cannot be lifted from an app; Android 14+ has a Developer Options toggle, and over adb `settings put global settings_enable_monitor_phantom_procs false`.

Not in the engine: SysV IPC (PostgreSQL), 32-bit guests, mount or user namespaces. `/proc/self/maps` and `mountinfo` still show host paths.

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
- **LSPs and debuggers.** Whatever you install in the rootfs is on the editor's PATH: rust-analyzer via rustup, gopls, pyright, clangd, jdtls. Zed's own managed Node runs vtsls, eslint and friends. CodeLLDB debugs Rust with real breakpoints.
- **Extensions.** Browse, install, manage. Themes, language configs, grammars, slash commands.
- **Remote SSH.** Server-picker pill in the title bar, persisted server list, native askpass.
- **Agents.** Zed's ACP agent UI, plus Zed's own agent with your API keys ([below](#ai)).
- **Multi-window.** Android freeform and DeX, each extra window is a real Activity with OS chrome.
- **Edge-to-edge** rendering with content under the display cutout.
- **App menu bar** with nested submenus (Settings, Keymap, Themes, Extensions).
- **Theme follow** for system light/dark.
- **`ZedDocumentsProvider`** exposes the project root as a SAF volume, so other Android apps can browse Zed's worktrees.

---

<a id="ai"></a>
## <img src="https://api.iconify.design/lucide:bot.svg?color=%23999999&height=22" valign="middle" /> &nbsp;AI with your own API keys

Besides ACP agents, Zed's own agent and inline assistant work with your API keys, no Zed account needed. Zdroid keeps the API providers Zed supports (Anthropic, OpenAI, Google, OpenRouter and the rest) and OpenAI-compatible endpoints such as a LiteLLM or vLLM gateway. It leaves out Zed-hosted models and web search, Zed sign-in, and the local model servers (Ollama, LM Studio, llama.cpp). ACP agents on an SSH remote keep their own authentication on the remote machine.

To add an OpenAI-compatible endpoint, open **Settings → AI → LLM Providers → Add Provider**, choose **OpenAI**, and enter the URL, API key and model name. Agent tools need a model with tool-call support. Keys are stored in Android Keystore. The same endpoint in `settings.json` (enter the key in LLM Providers):

```json
{
  "language_models": {
    "openai_compatible": {
      "my-gateway": {
        "api_url": "https://gateway.example.com/v1",
        "available_models": [{ "name": "my-model", "max_tokens": 128000 }]
      }
    }
  },
  "agent": {
    "default_model": { "provider": "my-gateway", "model": "my-model" }
  }
}
```

Requests come from the tablet, even in an SSH project, so the endpoint must be reachable from it. See [Zed's gateway documentation](https://zed.dev/docs/ai/use-a-gateway).

---

## <img src="https://api.iconify.design/lucide:hammer.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Build from source

You'll need:

- Rust toolchain with `aarch64-linux-android` (`rustup target add aarch64-linux-android`)
- [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk) (`cargo install cargo-ndk`)
- Android NDK r27 (`sdkmanager "ndk;27.0.12077973"`)
- Gradle 8+, `adb` on `$PATH`
- A device with USB or wireless debugging on (see "Debugging on the tablet" in [FORK.md](FORK.md))

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

Samsung Galaxy Tab S9 Ultra (Snapdragon 8 Gen 2 / Adreno 740, Android 16, One UI 8) is the daily driver, and release builds are compiled for its CPU. Other high-end devices from 2022 on should run them too (Snapdragon 8 Gen 1, Dimensity 9000, Exynos 2200, Tensor G3 and later), but older CPUs crash at launch, including the Tensor G2 in the Pixel 7 and Pixel Tablet. See [`docs/workarounds/reference-cpu-build.md`](crates/gpui_android/docs/workarounds/reference-cpu-build.md). Android 9+ and Vulkan 1.1 are required too, and only Adreno is exercised. Mali / Xclipse will run but may want shader tweaks.

A hardware keyboard is the supported config. Tablet plus Bluetooth keyboard, foldable in tablet mode, or DeX/desktop-mode with monitor and peripherals all work. Phones technically run but are de-prioritized; see [`docs/workarounds/deferred-phone-form-factor-polish.md`](crates/gpui_android/docs/workarounds/deferred-phone-form-factor-polish.md).

---

## <img src="https://api.iconify.design/lucide:file-text.svg?color=%23999999&height=22" valign="middle" /> &nbsp;License

GPL-3.0-or-later, same as upstream Zed. The engine crates `crates/zd_engine_core` and `crates/zd_loader` are MIT. The APK bundles the unmodified Ubuntu 26.04 base image (`ubuntu-base`, from [cdimage.ubuntu.com](https://cdimage.ubuntu.com/ubuntu-base/releases/26.04/release/)), whose packages carry their own licenses (GPL, LGPL, BSD, MIT and others); the source of every package is in the Ubuntu archive, and nothing in the image is modified.

© Dylan Murzello, distributed under GPL-3.0-or-later. Zed itself is © Zed Industries.

---

## <img src="https://api.iconify.design/lucide:handshake.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Acknowledgments

- [Zed Industries](https://zed.dev/) for [`gpui`](https://github.com/zed-industries/zed/tree/main/crates/gpui) being platform-agnostic enough that an Android port is plumbing rather than a rewrite.
- The [`wgpu`](https://github.com/gfx-rs/wgpu) and [`blade-graphics`](https://github.com/kvark/blade) maintainers for a Vulkan abstraction that just works on Adreno.
- [The Termux project](https://termux.dev/) for [a decade of Linux-on-Android](https://github.com/termux/termux-app), and [proot](https://proot-me.github.io/) for the path-translation design the engine descends from; [gVisor's systrap](https://gvisor.dev/) for showing that seccomp's trap return is a platform.
- [Ubuntu](https://ubuntu.com/) for publishing a base image small enough to ship inside an APK.

---

## <img src="https://api.iconify.design/lucide:circle-help.svg?color=%23999999&height=22" valign="middle" /> &nbsp;So why this ?

Zed Industries' position on a mobile/tablet port: **not planned**.

- [#12039 IOS/Android Port](https://github.com/zed-industries/zed/issues/12039), open since May 2024.
- [#34633 start of termux build](https://github.com/zed-industries/zed/issues/34633), closed as "not planned" in Jul 2025.
- [#43207 gpui: On Android](https://github.com/zed-industries/zed/issues/43207), open in the GPUI Roadmap as "Wide Scope" since Nov 2025.

This repo is what those threads were asking for, built independently. The Termux build attempt failed because the upstream `wasmtime`/`cranelift` deps don't compile inside Termux. We sidestep that by building the APK on a desktop with `cargo-ndk` and running a stock Ubuntu userland through our own ptrace-free chroot engine. The editor uses upstream Zed components with Android adaptations at the platform and runtime boundaries, documented in [`crates/gpui_android/docs/workarounds/`](crates/gpui_android/docs/workarounds/).
