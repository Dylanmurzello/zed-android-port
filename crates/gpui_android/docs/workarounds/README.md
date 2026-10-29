# Android port — workarounds library

Index of every non-obvious thing we do to make Zed run on Android. Each entry
points at a deeper writeup explaining the problem, the constraint that rules
out the obvious fix, and the chosen approach. **This file is the table of
contents; the writeups live alongside it.**

When a new constraint hits us and the workaround is non-trivial, add an entry
here so future sessions don't re-derive it from scratch. When a workaround
is removed, delete its writeup and its row; git history keeps them.

Features Zdroid implements because upstream Zed doesn't yet, to check at each
Zed sync, are listed in [ahead-of-upstream.md](../ahead-of-upstream.md).

## Reference hardware

Zdroid is developed and tested on a Samsung Galaxy Tab S9 Ultra, the tablet
Dylan used too (see "Tested on" in the top-level README). Unless a writeup
says otherwise, the observations and measurements here and in code comments
come from it: Snapdragon 8 Gen 2, Adreno 740 GPU, 120 Hz adaptive-refresh
panel, ~336 dpi, Android 16 with One UI 8, and the Book Cover Keyboard's
trackpad, in both tablet mode and DeX.

## How to read this

| Status | Meaning |
|---|---|
| **Active** | Currently shipping in the APK |
| **Deferred** | Designed, scheduled, not yet built |
| **Upstream** | Fixed in a dependency; what's left here is noted in the writeup |

## Storage / filesystem

| Workaround | Status | Why it exists |
|---|---|---|
| [Noexec mount on /storage/emulated/0](android-noexec-mount.md) | Active | FUSE-mounted shared storage rejects execve regardless of file mode |
| [~/storage curated symlinks](termux-storage-symlinks.md) | Active | Surface common /sdcard subdirs without making them workspace roots |
| [~/projects workspace + Import-from-sdcard](projects-workspace-import.md) | Active | Builds need exec mount; SAF picks land on noexec mount |
| [Copy projects from shared storage](noexec-banner-move.md) | Active | When users open from /sdcard anyway, copy the project into ~/projects |
| [Trust grants restored from WorkspaceDb at boot](trust-restore-from-db.md) | Active | Production main.rs does this; we forgot |
| [SAF picker integration](saf-picker-integration.md) | Active | gpui's prompt_for_paths needs Android Storage Access Framework + path-decoding gymnastics |

## Runtime engine

Every spawn routes through the rootfs engine: a static loader plus a
seccomp trap path translator that runs a stock Ubuntu userland.

| Workaround | Status | Why it exists |
|---|---|---|
| [Storage permission JNI shim](storage-permission-jni.md) | Active | Raw `/sdcard` paths need the runtime grants on Android 8 to 10 and All files access on 11+ |
| [Rootfs engine integration](rootfs-engine-integration.md) | Active | How the editor reaches the Ubuntu rootfs: the loader link, the bundled image extracted on first launch, the symlink farm, the spawn bridge, reset |
| [The guest environment contract](rootfs-guest-env.md) | Active | Editor keeps the host env, tools get the guest view at exec through `ZD_GUEST_ENV`; per-spawn PATH merge, terminal, SSH remotes |
| [Two spellings of the app files dir](android-files-dir-spelling.md) | Active | At targetSdk 35 the kernel reports `/data/data/...` for the files dir; the engine gets that spelling, the editor keeps `/data/user/0` |
| [The `/proc` files the app sandbox refuses](android-proc-restrictions.md) | Active | `stat`, `loadavg`, `uptime`, `net/*`, `sys/*` are EACCES for apps; the engine serves them from `sysinfo`, the clocks, `uname` and `/sys`; found through npm's silent no-op |
| [Pointer authentication keys and wasm fibers](android-pointer-authentication-fibers.md) | Active | From targetSdk 31 bionic gives every thread its own IA key; wasmtime fibers resumed on another worker died with a PAC exception; the IA key is disabled at boot |
| [JNI DNS bridge](jni-dns-bridge.md) | Active | Android has no `/etc/resolv.conf`; ConnectivityManager's servers seed the guest rootfs resolv.conf and follow network changes |
| [Asset selection by runtime OS](runtime-os-asset-selection.md) | Active | Downloaded tools run in the Linux userland, so every asset-selection site asks `util::runtime_os()` instead of `consts::OS` |
| [async-task: one vtable pointer per task waker](async-task-waker-identity.md) | Upstream | A task woke itself on every poll because its waker clones carried a different vtable address; Zed's pinned async-task has the fix, and the main loop's drain stays bounded |
| [Runtime foreground service and child reaping](runtime-foreground-service.md) | Active | Android stops an app's children once it is cached; the process tree under `hidepid` (or Keep Running in Background) drives a foreground service, and the reaping at quit |
| [Bounded language server auto-restart](android-lsp-auto-restart.md) | Active | A server killed from outside is restarted up to three times per ten minutes, then left stopped with a hint |

## Runtime env

| Workaround | Status | Why it exists |
|---|---|---|
| [CA certificates](ssl-cert-bundle.md) | Active | The editor's HTTPS uses webpki-roots; the runtime's tools use Ubuntu's `ca-certificates` |
| [.gitconfig safe.directory = *](gitconfig-safe-directory.md) | Active | The runtime's git sees every repository as another user's: the guest is root, the files the app's or media_rw's |
| [Activity-recreation idempotency](activity-recreation-idempotency.md) | Active | `android_main` re-enters; everything must be re-entrant |
| [SELinux context canary log](selinux-canary.md) | Active | Logs the app's SELinux domain at boot for reading spawn failures against it |
| [MultiWorkspace wrapper + load keymap last](multiworkspace-keymap-order.md) | Active | Workspace KeyContext + boot-order rules make keybindings actually fire |
| [Create worktree before attaching project panel](worktree-before-panel-attach.md) | Active | ProjectPanel::starts_open() needs the worktree present before add_panel runs |

## UI / input

| Workaround | Status | Why it exists |
|---|---|---|
| [Choreographer-driven vsync](choreographer-vsync.md) | Active | Replaces 8ms fixed-interval polling with vsync callbacks, requested only while gpui has frames to draw |
| [Two-finger tap → right click](two-finger-rightclick.md) | Active | Touchscreens don't have a right mouse button |
| [JVM stack overflow on clipboard](jvm-clipboard-stack-overflow.md) | Active | Android's 988KB android_main thread can't handle clipboard JNI synchronously |
| [first_mouse=false on Android touches](first-mouse-tagging.md) | Active | macOS-style first-click-focuses-the-window logic was no-op'ing every touch handler |
| [UI mode → window appearance](ui-mode-system-appearance.md) | Active | Hardcoded Light made dark-mode-following themes always pick One Light |
| [AssetSource threaded through gpui_android::run](assetsource-icons.md) | Active | Without it, every icon rendered as a blank rectangle |
| [PointerIcon JNI cursor mapping](pointer-icon-cursor-mapping.md) | Active | set_cursor_style was a no-op; mouse hover never showed I-beam, resize handles, etc. |

## Window / render

| Workaround | Status | Why it exists |
|---|---|---|
| [`platform.rs` no-drain RefCell pattern](refcell-drain-platform-bug.md) | Active | Draining main_receiver inside `open_window` panics on RefCell re-entry |
| [Block open_window until ANativeWindow ready](open-window-blocks-on-anativewindow.md) | Active | Renderer races against surface creation if we don't wait |
| [Construct buffer inside open_window](construct-buffer-inside-open-window.md) | Active | Sibling of refcell-drain; buffer constructor needs to live under the same borrow |
| [wgpu device-lost recovery](wgpu-device-lost-recovery.md) | Active | Android GPU driver loses context under memory pressure; need explicit drop+recreate |
| [Force a paint after surface attach](force-paint-after-surface-attach.md) | Active | Fresh swapchain doesn't get an invalidation event; force one explicit paint |
| [stacker reads the stack limit on Android](stacker-android-stack-limit.md) | Active | stacker couldn't read the stack limit on Android, so gpui's stack guard mapped a new stack on every guarded call |
| [Render-pipeline perf polish](deferred-render-pipeline-perf.md) | Deferred | 120Hz opt-in, Mailbox present mode, FrameMetrics instrumentation, ALooper spurious-wake hunt; "feels native-but-not-native" latency budget |

## Multi-window (L7)

| Workaround | Status | Why it exists |
|---|---|---|
| [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md) | Active | Each `cx.open_window` past the first becomes a separate Activity task → OS provides freeform chrome on DeX/desktop windowing |
| [`ZedApplication` for AppCompatActivity native lib load](zedapplication-loadlibrary.md) | Active | AppCompatActivity has no GameActivity meta-data hook → centralize `System.loadLibrary` in Application.onCreate |
| [JNI ClassLoader for app classes](jni-classloader-for-app-classes.md) | Active | `Class.forName` only sees framework classes; Activity's loader sees app classes |
| [JNI exception clear after error](jni-exception-clear-after-error.md) | Active | Pending Java exception from a failed JNI call aborts process on next JNI; must clear |
| [`futures::oneshot::Receiver::try_recv` semantics](futures-oneshot-tryrecv-semantics.md) | Active | `Ok(None)` means "not ready", not "channel dropped" — easy off-by-semantic bug |
| [Android 16 freeform configChanges (exhaustive)](android16-config-changes-resize.md) | Active | Drag-resize destroys Activity by default; declare every config to handle ourselves |
| [`appCategory="productivity"`](android16-app-category-productivity.md) | Active | Defang Android 16 games carve-out from desktop windowing — GameActivity inheritance could trip the heuristic |
| [`documentLaunchMode="always"` implies Intent flags](document-launch-mode-implies-flags.md) | Active | Setting both manifest mode AND explicit flags caused MainActivity to background under DeX |
| [Cold Activity launch timeout (4s)](activity-launch-cold-timeout.md) | Active | Cap the wait for an extra window's surface below the ANR threshold, and acknowledge the main Activity's pause while waiting (it caused 2 s freezes) |
| [Process-death recovery for extra windows](process-death-recovery-extra-windows.md) | Active | Activities resurrected from Recents after process kill must self-`finish()` if Rust runtime doesn't know their windowId |
| [`ActivityOptions.setLaunchBounds`](activity-options-launch-bounds.md) | Active | Pass gpui's `WindowParams.bounds` to the OS so freeform windows open at the requested size |
| [`with_active_or_new_workspace` falls back to existing on Android](with-active-or-new-workspace-android-fallback.md) | Active | When Settings is the active window, theme picker / command palette / recent projects routed through `with_active_or_new_workspace` were spawning duplicate Workspace ExtraWindowActivities; redirect to the existing primary instead |
| [`activate()` via `AppTask.moveToFront`](activate-extra-activity-move-to-front.md) | Active | `Window::activate_window()` was a no-op stub on Android; settings_ui's existing-window dedup needed it to surface a backgrounded Settings instead of re-opening |
| [`ZedDocumentsProvider` exposes the home folder as a SAF root](zed-documents-provider.md) | Active | Other apps couldn't browse / share files from `/data/data/<pkg>/files/home` via the system picker; ported Termux's provider shape with custom dev MIME map + search skip list + provider-pre-Activity `mkdirs` guard |
| [Notify `on_active_status_change` for cursor blink](notify-active-status-change.md) | Active | Editor's `cx.observe_window_activation` observer must fire to call `BlinkManager::enable`, otherwise search-bar cursor renders statically until first input; each window reports its Activity's focus so Zed's `CloseWindow` closes the window you're in |
| [No default focus highlight on extra-window surfaces](extra-window-focus-highlight.md) | Active | Android's keyboard-focus highlight covered the whole focusable surface as a whitish tint |

## Build / packaging

| Workaround | Status | Why it exists |
|---|---|---|
| [Debug-strip oversized .so](debug-strip-oversized-so.md) | Active | llvm-strip chokes on >2 GB ELF; profile.dev workaround |
| [audio + livekit + call cfg-gates](android-cfg-gates.md) | Active | These crates don't compile against bionic; mock fallbacks already exist |
| [Load bundled themes via LoadThemes::All](load-themes-all.md) | Active | Default lazy-load left the theme picker empty on Android |
| [Load default Linux keymap on boot](load-default-linux-keymap.md) | Active | Android KeyEvents look closer to Linux than macOS — pick the right keymap |
| [Build for the reference hardware's CPU](reference-cpu-build.md) | Active | The default target is Armv8.0; the tablet's CPU has faster atomics, but its kernel leaves SVE off |
| [Loader in `nativeLibraryDir` via extractNativeLibs](extract-native-libs-loader.md) | Active | The engine loader is exec'd by path from the APK's lib dir; AGP 8 keeps libs un-extracted by default |

## Adding a new workaround

1. Hit a constraint that needs non-obvious work to solve.
2. Solve it.
3. Add a row to the right table above with a one-liner.
4. Create the linked `.md` next to this file using the template below.
5. Link it from the corresponding commit message.

## Template

```markdown
# <Title>

**Status:** Active | Deferred | Upstream
**Phase / Commit:** <which session this came from>
**Files:** <paths>

## Problem
<what was broken — the symptom>

## Constraint
<what rules out the obvious fix; primary-source links if relevant>

## Solution
<our approach, with the smallest possible code excerpt>

## Why this works
<the load-bearing invariant>

## Failure mode if regressed
<concrete observable symptom>

## See also
<related workaround entries>
```
