# Zdroid fork

This repository is a fork of [Dylanmurzello/zed-android-port](https://github.com/Dylanmurzello/zed-android-port), Dylan Murzello's Android port of Zed. It merges each stable [Zed](https://github.com/zed-industries/zed) release, brings more of desktop Zed to Android, and publishes its own signed APKs. This section covers what the fork changes; Dylan's README follows below the line.

### Install

Download `Zdroid-X.Y.Z.apk` from [this fork's releases](https://github.com/ilkersigirci/zed-android-port/releases/latest). After that, Zdroid updates itself from the title bar.

> [!IMPORTANT]
> - This fork's APKs are signed with a different key than Dylan's. To switch, uninstall Dylan's build first. That deletes the app's data, including the Bootstrap runtime and its home folder.
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

- The Agent Panel runs ACP agents such as Claude Code, Codex and Antigravity, on SSH remotes and, after [installing their CLIs](#setting-up-your-shell-environment-bootstrap), locally under Bootstrap. Keep and Reject buttons review their edits.
- Zed's own agent and inline assistant work with your API keys: hosted providers such as Anthropic, OpenAI, Google and OpenRouter, and OpenAI-compatible endpoints ([details](#ai)).
- The screen stays on while an agent works.

**Touch, keyboard and pointer**

- Zed's touch gestures: momentum scrolling, double-tap to select a word, and two-finger swipes that scroll. A finger also drags pane splitters and dock edges. Tabs always show their close button.
- The on-screen keyboard behaves like other Android apps': it shows when you tap a text field or open a search box, not when you open a file, and the window's content stays above it. Its toggle is in the status bar.
- Long press in the terminal opens its context menu, with **Open in Browser** for a selected link.
- Ctrl+click, Shift+click and Alt+click work with a mouse or trackpad, Ctrl+tap opens links, and horizontal wheel scrolling works.
- Samsung DeX: clicks and hover land under the pointer in freeform windows. The Book Cover trackpad follows your finger in landscape.

**Windows**

- Ctrl+W and Ctrl+Shift+W in Settings and other pop-up windows close that window, not all of Zdroid. Closed windows leave Recents.
- Opening Settings no longer freezes for 2 seconds, windows no longer stay blank after the screen turns off, and pop-up windows no longer look washed out while a keyboard or mouse is in use.

**Battery and size**

- **Zdroid → Keep Running in Background** (optional) keeps terminals, SSH sessions and agents alive, with an ongoing notification that has a Stop action.
- Zdroid draws frames only when something changes and does less work in the background, so it uses much less battery while idle.
- Zed's release size settings, link-time optimization and packed relocations cut about 110 MB from the APK.

**Runtime and SSH**

- Bootstrap installs from Zdroid's fork, [`ilkersigirci/zdroid-bootstrap`](https://github.com/ilkersigirci/zdroid-bootstrap), or another repository you enter. The download is checked against the SHA-256 GitHub records, a dropped connection doesn't hang the install, and **Reinstall** moves to a newer release while keeping your home folder. `pkg upgrade` no longer breaks apt.
- SSH remotes download the remote server that matches Zdroid's Zed version, and remote terminals use the remote's own shell.
- The External Termux placeholder, which never ran anything, is gone.

**Updates, data and security**

- Updates work like desktop Zed's: an **Update Available** button in the title bar, hourly checks (`"auto_update": false` turns them off), download progress, and Android's install session API. An update must be signed with the release certificate.
- Credentials are stored in the Android Keystore. Plaintext ones from older versions are moved there.
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

Zdroid is an independent port of [Zed](https://zed.dev) for Android, not affiliated with Zed Industries. Zed's editing, workspace, search, Git, extension and terminal components run on a `gpui_android` platform backend with Android-specific adaptations and Vulkan rendering. The reference hardware and supported build configuration are documented in [the platform workarounds](crates/gpui_android/docs/workarounds/README.md). A downloaded Linux userland (Termux-derived, repackaged under `com.zdroid`) runs apt, bash, git, ssh, node, go and rust-analyzer as subprocesses from the app's private data directory; a second runtime adapter runs tools in a Kali chroot.

---

<p align="center">
  <img src="https://github.com/user-attachments/assets/68f763ba-e051-4217-8779-7bb9327f4a13" alt="Zdroid demo" width="100%" />
</p>

Vulkan via wgpu. AChoreographer-driven vsync, no JNI hop per frame. Opt-in 120Hz with Mailbox present mode. Glyph fallback into `/system/fonts` so Powerline arrows and CJK render without bundling fonts. Editing, projects, search, Git, extensions, and terminal UI use Zed's native implementations. The Rust `.so` is the app process. gpui composites every pixel (yes, you read that right) straight into the Adreno Vulkan driver. Multi-Activity OS-chromed extra windows so DeX freeform renders Settings and secondary editors with real chrome.

Termux userland under `com.zdroid`: packages from Termux's repository are repacked for it as they install, which works because `com.zdroid` is as long as `com.termux`. npm's Linux binaries, such as Claude Code and Codex, run on a bundled musl loader and read their DNS servers from `/sdcard/.zed/r`, which the app fills with Android's. Optional Magisk-flashable `zd-spawnd` daemon with SCM_RIGHTS stdio relay for the chroot runtime. SurfaceControl-composited hardware cursor sprite on a sibling overlay, separate from the wgpu frame. Pointer-capture trackpad that consumes historical motion samples so finger drags don't lose 80% of their travel to event batching. SAF DocumentsProvider exposing `~/` as a system volume. HTTPS trusts the Mozilla root bundle (`webpki-roots`). In-app updater pulling signed APKs from GitHub Releases. Everything else is upstream. Deep-dives for the platform layer live in [`crates/gpui_android/docs/workarounds/`](crates/gpui_android/docs/workarounds/).

---

## <img src="https://api.iconify.design/lucide:download.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Install

Grab the latest `Zdroid-X.Y.Z.apk` from the [releases page](https://github.com/Dylanmurzello/zed-android-port/releases/latest) and open it in your file manager. Android prompts for unknown-source installs the first time; grant it. Reinstalls upgrade in place because every release ships from the same signing cert.

> [!NOTE]
> Android may show a "built for an older version of Android" warning before you tap Install. Proceed anyway. `targetSdk` is pinned at 28 on purpose: the bundled Termux userland depends on the `untrusted_app_27` SELinux domain, which permits `execve` on app-private files. Bumping `targetSdk` to 29+ lands the process in a stricter domain that denies exec, and the entire runtime stops working. See [`docs/workarounds/targetsdk-28-execve.md`](crates/gpui_android/docs/workarounds/targetsdk-28-execve.md) for the receipts.

### First launch

1. **Storage permissions.** The system prompts for read/write to `/sdcard` so the editor can reach anything outside its app-private dir. Grant it. Without it, "Open Project" can't see your files.
2. **Runtime adapter.** Open **Configure runtime** in onboarding (or **Settings → Android** later). Choose **Bootstrap** for a Termux-derived userland without root, or **Kali chroot** for an existing Magisk and Kali NetHunter installation. The picker shows whether each runtime is ready.
3. **Bootstrap download.** Tap **Install** on the Bootstrap card to download the userland from [`ilkersigirci/zdroid-bootstrap`](https://github.com/ilkersigirci/zdroid-bootstrap), Zdroid's fork of [`Dylanmurzello/zdroid-bootstrap`](https://github.com/Dylanmurzello/zdroid-bootstrap). To use another repository that publishes `bootstrap-aarch64.zip` releases, enter it in **Download from** first. When it is ready, the card shows the installed release; select it and restart Zdroid. **Reinstall** on the card later moves to a newer release or another repository: it deletes the packages installed since, and keeps your home folder. Installation errors remain visible in the picker.

### Setting up your shell environment (Bootstrap)

Open the integrated terminal. First, sync the package index:

```sh
pkg update && pkg upgrade
```

Pre-baked in the bootstrap: rust-analyzer (the LSP binary; `cargo` and `rustc` are not bundled, install with `pkg install rust` if you want them), nodejs, go, bash, openssh, busybox, the bionic-compat patchelf, the hex-patched musl loader. `npm` is a separate Termux package and needs an install before user-facing `npm` calls work:

```sh
pkg install npm
```

The agent panel's Claude Agent and Codex start the Claude Code and Codex CLIs, which run on Android only from a global npm install: the bootstrap patches those. Install them, sign in from the terminal (`claude`, then `/login`; `codex login`), and restart Zdroid, which then points the agents at them:

```sh
npm install -g @anthropic-ai/claude-code @openai/codex
```

In the terminal, run Codex as `codex --no-daemon` (or add `alias codex='codex --no-daemon'` to `~/.bashrc`): by default it starts a background server that sandboxes itself with bubblewrap, which Android doesn't allow inside an app. The agent panel's Codex doesn't use that server.

Toolchains and LSPs for other languages have install recipes in the bootstrap repo: [`ilkersigirci/zdroid-bootstrap`](https://github.com/ilkersigirci/zdroid-bootstrap).

> [!NOTE]
> The first `pkg install` after extracting a fresh bootstrap will surface "broken dependencies" from apt and prompt you to run `apt --fix-broken install`. Run it. The pre-baked packages (`go`, `openssh`, busybox, etc.) declare dpkg dependencies that aren't formally registered in the database on a fresh extract, so apt flags the inconsistency the first time it has to resolve anything. fix-broken reconciles the state; `pkg` works normally afterward.

### Setting up Kali chroot

Prerequisites: a rooted device with Magisk installed.

1. Drop a Kali NetHunter aarch64 rootfs at `/data/local/nhsystem/kali-arm64`. NetHunter's installer is the standard path; any aarch64 Debian-derived rootfs at that location works.
2. Flash the [`zd-spawnd`](https://github.com/Dylanmurzello/zdroid-spawnd/releases) Magisk module. Reboot.
3. Open Zdroid and pick **Kali chroot** in the runtime picker.

Every subprocess the editor spawns (`bash`, `git`, LSPs, terminal shells) goes over a Unix socket to the `zd-spawnd` daemon, which `fork`s, `chroot`s, drops privileges, and `execve`s on your behalf. Sub-millisecond spawn versus ~200 ms for `su`-mediated alternatives. All the bionic-vs-glibc gotchas (`/usr/bin/env`, `/tmp`, `dlopen libfoo.so`) disappear because subprocesses run inside a real distro.

External Termux integration is unavailable. If an older version saved that placeholder choice, open **Settings → Android**, select Bootstrap or Kali chroot, and restart. Existing runtime settings are preserved when switching.

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
Settings, the runtime picker, and any other spawned window each open as their own Recents entries and get their own soft keyboard and input.

---

<a id="userland"></a>
## <img src="https://api.iconify.design/lucide:server.svg?color=%23999999&height=22" valign="middle" /> &nbsp;Userland

The editor is bionic-linked and runs as the Android app process. Every subprocess it spawns (`bash`, `apt`, language servers, formatters, terminal shells, `git`, `ssh`) routes through whichever runtime adapter the user picked. Two adapters ship; they version independently of the editor APK.

| Adapter | What it is | Where it comes from |
|---|---|---|
| **Bootstrap** _(no root)_ | Termux userland rebuilt under `com.zdroid`: apt/dpkg/bash with our package name baked into RUNPATHs and shebangs. Pure bionic, no glibc; same trade-offs as any Termux install. `apt` and `pkg install` work for everything Termux ships. | Downloaded from [`ilkersigirci/zdroid-bootstrap`](https://github.com/ilkersigirci/zdroid-bootstrap) using **Install** on the Bootstrap card in the runtime picker. |
| **Kali chroot** _(needs Magisk)_ | Real glibc Linux. Every spawn goes over a Unix socket to `zd-spawnd` (a small privileged daemon) which does `fork` + `chroot` + `setuid` + `execve` on the editor's behalf. ~5 ms per spawn vs ~200 ms for `su`-mediated. All the bionic gotchas (`/usr/bin/env`, `/tmp`, `dlopen libfoo.so`) disappear because subprocesses run inside a real distro. | Flash the Magisk module from [`Dylanmurzello/zdroid-spawnd`](https://github.com/Dylanmurzello/zdroid-spawnd), plus drop a Kali NetHunter aarch64 rootfs at `/data/local/nhsystem/kali-arm64`. |

Switching is one tap (Settings → Android). Selection persists in `$PREFIX/etc/zd-runtime.toml`. **Restart Zdroid after switching adapters.** The editor caches environment state (PATH, HOME, library search paths, spawn-router config) from whichever adapter was active at boot; without a restart, subprocesses spawned post-switch can land with stale env and fail in cryptic ways (LSPs not found, `git` claiming HOME doesn't exist, `pkg install` writing to the wrong rootfs, etc.).

### When to pick which

- **No root, just want it to work:** Bootstrap. apt, npm, `go install`, rust-analyzer all work. Rough edges: npm's Linux binaries (Claude Code, Codex) read their DNS servers from `/sdcard/.zed/r`, which the app fills from Android's at startup ([how it works](https://github.com/ilkersigirci/zdroid-bootstrap#dns)). Some glibc-only extension binaries don't run.
- **Have Magisk, want real Linux:** Kali chroot. Everything you'd expect on Debian/Kali works as-is, no shimming needed. The chroot is shared with whatever else uses that NetHunter rootfs.

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

GPL-3.0-or-later, same as upstream Zed. The Bootstrap-adapter zip (distributed from [`ilkersigirci/zdroid-bootstrap`](https://github.com/ilkersigirci/zdroid-bootstrap), not bundled in the APK) contains Termux-rebuilt packages each under its own license (mostly BSD/MIT/Apache; gnupg/bash/coreutils are GPL). The Alpine-derived `ld-musl-aarch64.so.1` inside it is MIT. The `zd-spawnd` daemon ([`Dylanmurzello/zdroid-spawnd`](https://github.com/Dylanmurzello/zdroid-spawnd)) is GPL-3.0-or-later.

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

This repo is what those threads were asking for, built independently. The Termux build attempt failed because the upstream `wasmtime`/`cranelift` deps don't compile inside Termux. We sidestep that by building the APK on a desktop with `cargo-ndk` and running our own custom Termux userland in process. The editor uses upstream Zed components with Android adaptations at the platform and runtime boundaries, documented in [`crates/gpui_android/docs/workarounds/`](crates/gpui_android/docs/workarounds/).
