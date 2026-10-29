# Android port — workarounds library

Index of every non-obvious thing we do to make Zed run on Android. Each entry
points at a deeper writeup explaining the problem, the constraint that rules
out the obvious fix, and the chosen approach. **This file is the table of
contents; the writeups live alongside it.**

When a new constraint hits us and the workaround is non-trivial, add an entry
here so future sessions don't re-derive it from scratch. When a workaround
is replaced by something cleaner, mark it Reverted and link to the
replacement.

## How to read this

| Status | Meaning |
|---|---|
| **Active** | Currently shipping in the APK |
| **Deferred** | Designed, scheduled, not yet built |
| **Reverted** | Was active, replaced by something cleaner |
| **Superseded** | Replaced by the rootfs engine; kept for history |

## Storage / filesystem

| Workaround | Status | Why it exists |
|---|---|---|
| [Noexec mount on /storage/emulated/0](android-noexec-mount.md) | Active | FUSE-mounted shared storage rejects execve regardless of file mode |
| [`targetSdk=28` for execve from /data/data](targetsdk-28-execve.md) | Reverted | The engine execs only the loader in the native library dir, so the pin bought nothing; targetSdk is 35 |
| [Termux ~/storage curated symlinks](termux-storage-symlinks.md) | Active | Surface common /sdcard subdirs without making them workspace roots |
| [~/projects workspace + Import-from-sdcard](projects-workspace-import.md) | Active | Builds need exec mount; SAF picks land on noexec mount |
| [Noexec banner with one-tap Move action](noexec-banner-move.md) | Active | When users open from /sdcard anyway, fix in one tap |
| [Trust grants restored from WorkspaceDb at boot](trust-restore-from-db.md) | Active | Production main.rs does this; we forgot |
| [Welcome-page Workspace/External split](welcome-page-split.md) | Active | Two `rust` projects from different storage tiers were indistinguishable |
| [SAF picker integration](saf-picker-integration.md) | Active | gpui's prompt_for_paths needs Android Storage Access Framework + path-decoding gymnastics |
| [Tier 2 root storage (bind-mount /mnt/pass_through)](deferred-tier2-root-storage.md) | Deferred | Wait for in-app settings UI |
| [`CARGO_TARGET_DIR` stopgap](reverted-cargo-target-dir.md) | Reverted | Per-tool env redirect didn't generalize |

## Runtime engine

The bootstrap-era workarounds (dpkg path-rewrite, apt hooks, musl loader
bundle, node `NODE_PLATFORM` patch, npm intercept stack, resolv.conf
hex-patch, patchelf + bun-binary classification) went away with the
bootstrap adapter; their writeups stay in
**[`Dylanmurzello/zdroid-bootstrap/docs/`](https://github.com/Dylanmurzello/zdroid-bootstrap/tree/main/docs)**
for history. Every spawn now routes through the rootfs engine (a static
loader plus a seccomp trap path translator); its writeups land here as it
is built.

| Workaround | Status | Why it exists |
|---|---|---|
| [Storage permission JNI shim](storage-permission-jni.md) | Active | Raw `/sdcard` paths need the runtime grants on Android 8 to 10 and All files access on 11+ |
| [Rootfs engine integration](rootfs-engine-integration.md) | Active | How the editor reaches the Ubuntu rootfs: the loader link, the bundled image extracted on first launch, the symlink farm, the spawn bridge, reset |
| [The guest environment contract](rootfs-guest-env.md) | Active | Editor keeps the host env, tools get the guest view at exec through `ZD_GUEST_ENV`; per-spawn PATH merge, terminal, SSH strip list |
| [Two spellings of the app files dir](android-files-dir-spelling.md) | Active | At targetSdk 35 the kernel reports `/data/data/...` for the files dir; the engine gets that spelling, the editor keeps `/data/user/0` |
| [The `/proc` files the app sandbox refuses](android-proc-restrictions.md) | Active | `stat`, `loadavg`, `uptime`, `net/*`, `sys/*` are EACCES for apps; the engine serves them from `sysinfo`, the clocks, `uname` and `/sys`; found through npm's silent no-op |
| [Pointer authentication keys and wasm fibers](android-pointer-authentication-fibers.md) | Active | From targetSdk 31 bionic gives every thread its own IA key; wasmtime fibers resumed on another worker died with a PAC exception; the IA key is disabled at boot |
| [Runtime config location + not-configured state](runtime-config-migration.md) | Superseded | One runtime, no selection file; the sentinel is the only state |
| [JNI DNS bridge](jni-dns-bridge.md) | Active | Android has no `/etc/resolv.conf`; ConnectivityManager's servers seed the guest rootfs resolv.conf |
| [Asset selection by runtime OS](runtime-os-asset-selection.md) | Active | Downloaded tools run in the Linux userland, so every asset-selection site asks `util::runtime_os()` instead of `consts::OS` |
| [Vendored async-task: one vtable pointer per task waker](async-task-waker-identity.md) | Active | A task woke itself on every poll because its waker clones carried a different vtable address; the SSH connect task froze the main loop |
| [Runtime foreground service and child reaping](runtime-foreground-service.md) | Active | Android stops an app's children once it is cached; the process tree under `hidepid` drives a foreground service and the reaping at quit |
| [Bounded language server auto-restart](android-lsp-auto-restart.md) | Active | A server killed from outside is restarted up to three times per ten minutes, then left stopped with a hint |
| [`/sdcard/.zed/` namespace](sdcard-dot-zed-namespace.md) | Reverted | Only existed for the hex-patched musl loader's 14-byte path slot; nothing is hex-patched anymore |
| [Bun resolv.conf hex-patch resilience](deferred-bun-resolv-resilience.md) | Superseded | Hex-patching binaries is gone; guest binaries read a real `/etc/resolv.conf` |
| [LD_PRELOAD strip for musl-static children](deferred-musl-subprocess-ld-preload.md) | Superseded | No LD_PRELOAD shim anymore; the engine handles shebangs at exec |
| [LD_PRELOAD `libzed-compat.so` path-redirect shim](deferred-ld-preload-shim.md) | Dropped | Doesn't apply to static-musl (no PLT/GOT); replaced by hex-patch in the bootstrap musl loader |

## Runtime env

| Workaround | Status | Why it exists |
|---|---|---|
| [HOME env dual-pointing](home-env-dual-pointing.md) | Superseded | The guest view (`HOME=/root`) is applied by the loader at exec; see the guest environment contract |
| [Terminal HOME override](terminal-home-override.md) | Superseded | Same: no `TERMUX__HOME` anymore |
| [SSL_CERT_FILE / CURL_CA_BUNDLE](ssl-cert-bundle.md) | Removed | In-process trust via webpki-roots; subprocess CA is adapter-owned |
| [.gitconfig safe.directory = *](gitconfig-safe-directory.md) | Active | libgit2 dubious-ownership check fires for media_rw-owned /sdcard repos |
| [Activity-recreation idempotency](activity-recreation-idempotency.md) | Active | `android_main` re-enters; everything must be re-entrant |
| [SELinux context canary log](selinux-canary.md) | Active | Logs the app's SELinux domain at boot for reading spawn failures against it |
| [MultiWorkspace wrapper + load keymap last](multiworkspace-keymap-order.md) | Active | Workspace KeyContext + boot-order rules make keybindings actually fire |
| [Create worktree before attaching project panel](worktree-before-panel-attach.md) | Active | ProjectPanel::starts_open() needs the worktree present before add_panel runs |

## UI / input

| Workaround | Status | Why it exists |
|---|---|---|
| [Choreographer-driven vsync](choreographer-vsync.md) | Active | Replaces 8ms fixed-interval polling with event-driven vsync |
| [Two-finger tap → right click](two-finger-rightclick.md) | Active | Touchscreens don't have a right mouse button |
| [JVM stack overflow on clipboard](jvm-clipboard-stack-overflow.md) | Active | Android's 988KB android_main thread can't handle clipboard JNI synchronously |
| [Soft-keyboard / IME bridge](deferred-soft-keyboard.md) | Deferred | Hardware keyboard works; touch IME bridge is its own engineering problem |
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
| [Cold Activity launch timeout (4s)](activity-launch-cold-timeout.md) | Active | Cold ExtraWindowActivity start ~2s; cap synchronous wait below ANR threshold |
| [Process-death recovery for extra windows](process-death-recovery-extra-windows.md) | Active | Activities resurrected from Recents after process kill must self-`finish()` if Rust runtime doesn't know their windowId |
| [`ActivityOptions.setLaunchBounds`](activity-options-launch-bounds.md) | Active | Pass gpui's `WindowParams.bounds` to the OS so freeform windows open at the requested size |
| [`with_active_or_new_workspace` falls back to existing on Android](with-active-or-new-workspace-android-fallback.md) | Active | When Settings is the active window, theme picker / command palette / recent projects routed through `with_active_or_new_workspace` were spawning duplicate Workspace ExtraWindowActivities; redirect to the existing primary instead |
| [`activate()` via `AppTask.moveToFront`](activate-extra-activity-move-to-front.md) | Active | `Window::activate_window()` was a no-op stub on Android; settings_ui's existing-window dedup needed it to surface a backgrounded Settings instead of re-opening |
| [`ZedDocumentsProvider` exposes `~` as a SAF root](zed-documents-provider.md) | Active | Other apps couldn't browse / share files from `/data/data/<pkg>/files/home` via the system picker; ported Termux's provider shape with custom dev MIME map + search skip list + provider-pre-Activity `mkdirs` guard |
| [Notify `on_active_status_change` for cursor blink](notify-active-status-change.md) | Active | Editor's `cx.observe_window_activation` observer must fire to call `BlinkManager::enable`, otherwise search-bar cursor renders statically until first input |

## Build / packaging

| Workaround | Status | Why it exists |
|---|---|---|
| [Debug-strip oversized .so](debug-strip-oversized-so.md) | Active | llvm-strip chokes on >2 GB ELF; profile.dev workaround |
| [audio + livekit + call cfg-gates](android-cfg-gates.md) | Active | These crates don't compile against bionic; mock fallbacks already exist |
| [Load bundled themes via LoadThemes::All](load-themes-all.md) | Active | Default lazy-load left the theme picker empty on Android |
| [Load default Linux keymap on boot](load-default-linux-keymap.md) | Active | Android KeyEvents look closer to Linux than macOS — pick the right keymap |
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

**Status:** Active | Deferred | Reverted | Superseded
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
