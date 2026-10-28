# Zdroid backlog

Remaining Android feature gaps, device reports and unfixed review findings.

## "Open Project" on first boot: SAF picker works, post-pick handoff drops the project on the floor

**Repro:** fresh install (or after `pm clear com.zdroid`). Go through onboarding, tap **Finish Setup**. Welcome page appears. Tap **Open Project**, SAF picker opens normally, pick a folder, return to the editor. **Nothing visibly changes** (still on Welcome). Close the app and reopen it; now the picked project loads and works normally.

So the action dispatch + SAF picker + Java -> Rust callback all work. The break is somewhere in the post-pick "replace this window's workspace with the new project" path.

**Code path** (after the SAF URI comes back via `Java_com_zdroid_MainActivity_onPickerResult` in `saf.rs:126`):

```
prompt_for_open_path_and_open                                   workspace.rs:721
  paths.await -> Some(picked)
  multi_workspace_handle = window.window_handle().downcast::<MultiWorkspace>()
  if !create_new_window && multi_workspace_handle.is_some():
      multi_workspace.open_project(paths, OpenMode::Activate)   multi_workspace.rs:1933
        if multi_workspace_enabled(cx):                          (true when agent settings enabled, default)
            find_or_create_local_workspace(...)                  multi_workspace.rs:1968
              -> Workspace::new_local(... OpenMode::Activate)
                -> [either reuse current window OR spawn new one
                    depending on serialized_workspace and the
                    1955 vs 1995 branch in workspace.rs]
            detach_workspace(empty_welcome_workspace)
```

**Likeliest cause:** `find_or_create_local_workspace` creates the new project workspace in a fresh MultiWorkspace window (= new `ExtraWindowActivity` on Android) instead of reusing the current MainActivity-hosted Welcome window. The Welcome window's empty workspace gets `detach_workspace`'d. The user is staring at MainActivity (which still has the now-detached Welcome surface) while the new project window opens off-screen / behind / in another freeform tile they don't notice.

That explains why "close + reopen" works: workspace-restore at next launch loads the most-recently-activated workspace state (= the project that was opened), so it materializes in the front MainActivity that time.

Adjacent symptom of the same class: the `with-active-or-new-workspace-android-fallback` workaround doc covers an earlier instance of this multi-Activity routing problem for action-dispatched modals (theme picker, command palette).

**Other possibilities to rule out with logs:**

1. `multi_workspace.open_project` returns Err silently (the `.log_err()` on line 763 of workspace.rs would swallow it).
2. The new workspace IS created in the current MultiWorkspace but `activate()` on line 1983 fails to switch the active workspace pointer, so the UI keeps rendering the empty Welcome.
3. The current MultiWorkspace's `active_workspace` was already replaced but the underlying `ExtraWindowActivity` SurfaceView isn't getting a fresh draw call.

**Diagnostic next step:** capture `adb logcat --pid=$(adb shell pidof com.zdroid)` from app launch through the failed Open Project flow. The signals to look for:

- `AndroidPlatform::prompt_for_paths invoked` -> action reached platform.
- `saf: pick_folder requested` then `saf: MainActivity.launchOpenTree() returned` -> SAF Intent dispatched cleanly.
- `saf: onPickerResult uri=...` -> Java callback fired with the picked URI.
- After that, anything mentioning `open_project`, `find_or_create_local_workspace`, `activate`, or new-window creation (search for `cx.open_window`, `ExtraWindowActivity` in logs) tells us whether a new window was spawned or the existing one was reused.

If we see a new-window spawn line, the fix is to force `OpenMode::Activate` to reuse the current MultiWorkspace's window on Android (probably in `multi_workspace.rs:open_project`, branch on `cfg(target_os = "android")` and pick a different path that doesn't call `find_or_create_local_workspace`).



## Reported via the @-mention bug-can't-be-filed loop (2026-05-13)

External user tried to file these and was blocked by upstream Zed's `blank_issues_enabled: false` + Discord-redirect contact_links (fixed in `244f29a455`). Logging here so they don't get lost while the user re-files via the new templates.

- **Mouse scroll direction reversed**: Samsung One UI + Bluetooth mouse. Scroll works correctly everywhere else on the device but is inverted in Zdroid. Likely a sign flip in `gpui_android`'s `MotionEvent` → scroll-delta translation. Check the wheel-axis path specifically (touch-pad two-finger scroll may differ); One UI may report `AXIS_VSCROLL` with a sign that mainline Zed doesn't compensate for. The uncaptured mouse-wheel path now passes vertical deltas through; verify captured and uncaptured scrolling on One UI before treating the original device report as resolved.
- ~~**Vim mode broken**~~ — **fixed in 29b29ddd63** (key events now route from `ExtraWindowActivity` to gpui via `nativeOnExtraKeyEvent` JNI bridge; the root cause was that any extra window's `dispatchKeyEvent` was never forwarded so `PlatformInput::KeyDown` never fired). Vim and any other editor in a non-Main window now accepts typing.
- ~~**Settings searchbar typing broken**~~ — **fixed in 29b29ddd63** (same key-routing fix, plus a tap-to-focus wrapper on the search bar with `stop_propagation` so taps don't bubble to `SettingsWindow`'s focus tracker which was immediately blurring the bar).

## Runtime picker and Settings search: resolved implementation gaps

The runtime picker now shows health badges and explanations, installs Bootstrap on request, links to the chroot module, and requires a restart after selection. The 2026-10-28 review removed the unusable External Termux entry, preserved saved adapter settings when switching, and made install/save errors persistent in the picker. Chroot setup remains external to the app.

Open: Select is disabled for a Bootstrap the health check calls Misconfigured (`proot_rootfs` set but proot or the rootfs missing), and the picker offers no other action there, so the fix is editing `zd-runtime.toml` by hand. Before 0.9.17, selecting rewrote the file with defaults, which cleared `proot_rootfs`.

Settings search input was fixed in `29b29ddd63`, and text fields now request GPUI's virtual keyboard.

## Image paste into the agent panel doesn't work on Android

**Repro:** copy an image to the Android clipboard (screenshot share-sheet "Copy to clipboard", or copy from Files / Gallery / a browser). Open the agent panel in Zdroid, focus the message editor, paste. Nothing happens. Text paste works fine.

**Diagnosis:** not a permission or storage gap. The Android clipboard bridge at `crates/gpui_android/src/clipboard.rs` is text-only by design; the module doc calls this out explicitly (`ClipboardItem` can carry images and structured entries, but our editor only round-trips text through it). `read()` calls `getPrimaryClip()` and pulls `getText()` off item 0; any `image/*` MIME entries on the clip are silently discarded before they ever reach gpui.

**Consumer:** the agent panel's message editor at `crates/agent_ui/src/message_editor.rs:278` is the only path that consumes `ClipboardEntry::Image`; `crates/agent_ui/src/mention_set.rs:931` is the same hook from the mention resolver. The editor itself doesn't accept image paste, so the wiring only matters when the agent panel is focused.

**Wiring needed:**

1. Extend `clipboard::read_inner` to inspect `ClipDescription.getMimeType(i)` for each item. If `image/png`, `image/jpeg`, `image/webp`, or `image/gif`, treat it as image instead of falling through to `getText()`.
2. Pull the `content://` URI from `ClipData.Item.getUri()` and open it via `ContentResolver.openInputStream(uri)`. The clipboard system auto-grants read access to the receiving app for as long as the clip is active, so no `FLAG_GRANT_READ_URI_PERMISSION` plumbing on our side.
3. Read the full byte stream, map MIME to `gpui::ImageFormat`, return `ClipboardEntry::Image(Image::from_bytes(format, bytes))`. `gpui::Image` is just `format + bytes`, no decode required on the platform side.

**Implementation constraints:**

- The existing `read()` runs on the gpui render thread with a 50ms JNI cache because cx.read_from_clipboard is polled at 60fps for the Paste menu's enabled state. Reading multi-megabyte screenshots through JNI on the render thread risks the same `android_main` stack overflow the doc warns about. Push image reads onto the 2MB-stack worker pattern already used by `write_on_worker` and stash the resolved `Image` in the cache so subsequent polls don't re-read.
- Cache invalidation: bumping the read cache TTL or attaching a clipboard-change listener (`OnPrimaryClipChangedListener` via JNI) so we know when to drop the cached image without polling MIME types every 50ms.
- Decoding format mapping: prefer the MIME from `ClipDescription`; fall back to magic-byte sniffing if absent (rare on Android but Termux clipboard managers sometimes paste raw bytes without a description).

**Adjacent (not yet needed):** write path for images. Copy-image-out is much less common in editor flows; only worth wiring once a consumer exists.

**Scope estimate:** ~200 LoC in `crates/gpui_android/src/clipboard.rs` plus ~30 LoC of JNI helpers. `ContentResolver.openInputStream` is one of the fiddlier JNI surfaces; the `InputStream` object needs an `available()` + `read(byte[])` loop or `Channels.newChannel`. Single-file change otherwise. Half a day if the JNI byte-read path lines up first try, full day if we trip ANR-territory issues on big screenshots.

**Why deferred:** core editor flow works without it, and the agent panel feature gating depends on `supports_images` from the LLM provider anyway. Pick up once a user reports a concrete blocked workflow.

## Complete native Zed AI integration

The app initializes upstream `agent_ui` and `acp_tools`, including the agent panel and context-server UI; `agent`, `acp_thread`, and `context_server` are transitive dependencies.

Since 0.9.17 the app also runs Zed's model providers (`copilot_chat::init`, `language_model::init`, `language_models::init`, `web_search::init`, in the order of Zed's `main()`), so the native agent and the inline assistant work with your own API keys, including OpenAI-compatible endpoints (a LiteLLM gateway, for example). An Android-gated edit to `language_models::register_language_model_providers` leaves out the local model servers (Ollama, LM Studio, llama.cpp); a remote one can be added as an OpenAI-compatible provider. The gate has to be at registration: user-defined compatible providers can share those IDs, so unregistering afterwards would remove them too. Zed accounts stay out: the app skips `client::init` (sign-in actions), `RefreshLlmTokenListener`, `web_search_providers::init` and `copilot_ui::init`, and three Android-gated edits to Zed's crates hide the rest: `language_models::register_language_model_providers` skips the Zed-hosted provider (constructing it needs `RefreshLlmTokenListener`, which panics when it isn't registered), `ai_onboarding::AgentPanelOnboarding` shows only the API-key setup, and `AgentPanel::should_render_new_user_onboarding` ignores empty history. Zed doesn't call `language_models::init_extension_proxy` at v1.26.2, so extension-provided model providers stay off here too; enable them when Zed does.

Still unverified on a device: streamed responses and tool calls through an OpenAI-compatible endpoint, inline edits, API keys surviving a restart (Android Keystore), Copilot Chat's GitHub sign-in, and local MCP servers under Bootstrap and chroot (including the sandbox fallback), as opposed to servers on an SSH host. Copilot edit prediction is not initialized. Android's API-key card in a native agent thread has no dismiss button (Zed's card has one through `ZedAiOnboarding`), so it stays until a provider is configured.

**External agents under Bootstrap (found 2026-10-28).** Claude Agent and Codex come from the ACP registry as npm packages, which Zed installs with the runtime's npm into `external_agents/registry/npx/` under its data directory (`LocalRegistryNpxAgent` in `agent_server_store.rs`). Both adapters run on Node but start a native CLI from their own `node_modules`: a musl build of Claude Code, and a static musl build of Codex. Bootstrap's `zed-launcher-gen.sh`, which its npm wrapper runs after every install, only patches binaries under `$PREFIX/lib/node_modules`: the musl loader path, the `/etc/resolv.conf` DNS patch and the `$PREFIX/bin` wrapper that drops `LD_PRELOAD`. The copies Zed installs miss all three, and the adapters inherit `LD_PRELOAD` from `npm_command_env`, so Claude Code likely can't start, and Codex can't resolve hostnames: on the tablet its thread spins forever, with Codex retrying DNS against 127.0.0.1 and never opening a connection. Since 0.9.19 the Bootstrap adapter sets `CLAUDE_CODE_EXECUTABLE` and `CODEX_PATH` to `$PREFIX/bin/claude` and `$PREFIX/bin/codex` when they exist at startup, so `npm install -g @anthropic-ai/claude-code @openai/codex` and a restart are enough. Running the launcher generator over Zed's agent directory too would drop the global installs, but Claude Code would still need `LD_PRELOAD` removed.

Tried on the tablet on 2026-10-28 with the workaround: the Codex thread opened, then every request failed with "workspace routing discovery failed". Codex looks for a CA bundle in Linux paths Android doesn't have, and Bootstrap set `SSL_CERT_FILE` only in the terminal; since 0.9.19 it sets it for the whole app, so agents and language servers get it too; the tablet confirmed the fix. Claude Agent is still untested.

## Scrolling runs at about 64 fps on the 120 Hz panel

Measured on 2026-10-29 on 0.9.21, scrolling a file in landscape with `adb shell input swipe`: a frame every 15.6ms on average, and 83% of the gaps between frames were longer than one refresh. `android_main` spends about 12.5ms of CPU per frame on the prime core at its maximum clock, so the CPU scheduler can't help; a performance hint session changed nothing (`crates/gpui_android/docs/workarounds/choreographer-vsync.md`). The 2026-10-26 profile put the time in gpui's text painting (glyph raster bounds, atlas lookups, sprite sorting) and wgpu's command encoding. Next: profile scrolling with the build's native symbols, and look for work Android does per frame that desktop doesn't, before changing gpui itself.

## Platform gaps found in the 2026-10-28 audit

- **A second clipboard write can be lost.** `clipboard.rs::write` returns early while `WRITE_IN_FLIGHT` is set, so copying twice in quick succession leaves the first text on the clipboard. Queue the latest write instead of dropping it. Tablet check: copy two different selections back to back, then paste.
- **`prompt_for_paths` ignores `PathPromptOptions`.** It always opens the SAF folder picker (`ACTION_OPEN_DOCUMENT_TREE`), so requests for files or multiple paths get a single folder.
- **`reveal_path` and `open_with_system` are empty** in `platform.rs`, so "Reveal in file manager" and "Open with system app" do nothing.
- **"Failed to run package-version-server" in the status bar.** Zed's JSON support downloads `package-version-server`, which adds version hints to `package.json`, and stops at "Running on unsupported os: android" (`languages/src/json.rs`, `NodeVersionAdapter`). Harmless, but shown whenever a JSON file opens. Fix: leave it out of the JSON language servers on Android.

## Open findings from the 2026-10-26 and 2026-10-27 reviews

The Android audit, the review of that week's PR stack and the two upstream-divergence audits found these. They were only in session notes until 2026-10-28, when each was rechecked against the code.

- **The keyboard button's state is guessed, not read.** `requestIme` in `MainActivity` and `ExtraWindowActivity` sets `imeShown` as soon as it calls `showSoftInput`, and `confirmImeShown` clears it again if the keyboard hasn't appeared after a delay. Android reports whether the keyboard is up in the insets both Activities already receive (`WindowInsetsCompat.isVisible(Type.ime())`): set `imeShown` only from the insets listener and drop the timer. Tablet check: show and hide the keyboard by tapping text, with Back and with the status bar button, in the main window and in Settings; the button and the programming extras row must always match the keyboard.
- **The DNS file for Bun-based CLIs goes stale.** `dns_bridge::populate_resolv_conf` writes `/sdcard/.zed/r`, which the patched `claude` and `codex` read instead of `/etc/resolv.conf`, once at startup (`lib.rs`), right after the asynchronous storage-permission request. On first launch it usually fails for lack of that permission, and it's never rewritten when the network changes, so those CLIs keep the previous network's resolver. Fix: rewrite it from `ConnectivityManager.registerDefaultNetworkCallback` (`onLinkPropertiesChanged`) and once the storage permission is granted. Tablet check: with Zdroid running, move from Wi-Fi to a phone hotspot, then run `claude` in the terminal.
- **Extra windows return from `surfaceDestroyed` before the renderer stops.** `ExtraWindowActivity`'s `surfaceDestroyed` only queues `nativeOnExtraSurfaceDestroyed` (`multi_window.rs`), so gpui can still present to a surface Android has destroyed, which the `SurfaceHolder` contract forbids (`NativeBridge.kt` says Rust must stop inside the callback); wgpu then reports errors to its uncaptured-error handler. Fix: block, with a timeout, until the gpui thread confirms the detach, and handle that event in `create_extra_window_blocking`'s wait loop so the two can't deadlock. Tablet check: in DeX, minimize and restore the Settings window repeatedly and look for wgpu errors in logcat.
- **Restoring the last session picks the main window by timestamp.** gpui_android doesn't implement `Platform::window_stack`, so the restore in `lib.rs` takes the window whose workspace was saved last. That column has one-second precision, so two windows saved in the same second come back in no fixed order, and the main Activity can restore the other window's workspaces. Fix: implement `window_stack()` with the main window first, or persist the main window's id. Tablet check: in DeX, open a second workspace window, edit in both, force-stop and relaunch; the main window must come back with its own workspace.
- **SSH: `-f` leaves master processes behind, and `accept-new` trusts new hosts.** On Android, `remote/src/transport/ssh.rs` adds `-f`, so ssh moves the master connection into a background process Zed doesn't track. `kill()` only kills the parent, which has already exited, so every disconnect or reconnect leaves an ssh master running. The comment blames zd-exec for keeping stdout open: fix that in zd-exec, or have `kill()` run `ssh -O exit`, then drop `-f` and the exit-status edits that exist for it. `StrictHostKeyChecking=accept-new` accepts any unknown host key without asking. Zed sets `SSH_ASKPASS_REQUIRE=force` and the app bundles an askpass helper, so OpenSSH may already ask through it: test without the option, and drop it if the prompt appears. Tablet check: disconnect and reconnect an SSH project a few times, then count ssh processes with `adb shell ps -A | grep ssh`; connect to a new host and see whether Zed asks about its key.

## 0.9.21 device checks

0.9.21 merges Zed v1.26.2. Zed now reads an SSH remote's environment by piping its script to `sh -s` over ssh's stdin instead of passing it as an argument. Check:

- Open an SSH project from a new connection: it connects, and the terminal and an agent thread start on the remote.

## 0.9.20 device checks

0.9.20 removes a closed extra window's card from Recents. Check in DeX or desktop windowing:

- Close Settings and the runtime picker with the window's X, with Back and with Ctrl+W: none leaves a card in Recents.
- Tapping a leftover card from an older version removes it instead of opening an empty window.

## 0.9.19 device checks

0.9.19 installs the latest release of Zdroid's fork of Bootstrap, checked against the SHA-256 GitHub records for it, adds the picker's **Download from** field and **Reinstall**, and gives the whole app Bootstrap's CA bundle. That fork's releases since `v2026.05.06-r6` keep apt working through `pkg upgrade` ([zdroid-bootstrap#1](https://github.com/Dylanmurzello/zdroid-bootstrap/issues/1)); `ilkersigirci/zdroid-bootstrap`'s test workflow checks that in Termux's Docker image, which runs Termux's binaries with Android's linker. Check on the tablet:

- On a fresh install, `npm install -g @anthropic-ai/claude-code @openai/codex` installs both CLIs' native binaries: `claude --version` and `codex --no-daemon --version` work. Then, with no `agent_servers` settings, after a restart, a Claude Agent thread answers. Codex passed this on 2026-10-28 (installed with `npm_config_os=linux`, which r7's npm wrapper now sets).
- On a fresh install (or another device), the Bootstrap card shows **Download from** with `ilkersigirci/zdroid-bootstrap`, and Install succeeds; the log shows "verified bootstrap-aarch64.zip" with the SHA-256 GitHub recorded, and the card then shows "Installed: <the latest tag> from ilkersigirci/zdroid-bootstrap". An existing install shows only its tag. Typing `Dylanmurzello/zdroid-bootstrap` installs his latest release instead, and a typo shows the field's error.
- With Bootstrap installed, **Reinstall** shows **Download from** and a warning; **Delete and reinstall** replaces the runtime: packages installed since are gone, the home folder and runtime choice stay, the banner asks for a restart, and after it the terminal works. **Cancel** changes nothing.
- After a reinstall, `pkg update && pkg upgrade` (with `apt --fix-broken install` when apt asks for it, then `pkg upgrade` again) keeps apt working: `pkg install jq` and `jq --version` succeed afterwards.
- rust-analyzer's first `cargo metadata` in a new project downloads the crates.io index without a certificate error.

## 0.9.18 device checks

0.9.18 moved the Android spawn hooks out of Zed's crates into `zdroid_hooks` and trimmed the Settings and welcome page edits. Check on the tablet, with Bootstrap and with the chroot:

- The terminal opens in the runtime's home with its PATH; `git status`, a language server and `npm install` of a package with native binaries work in a project under `~/projects`, and a language server starts for a project on shared storage.
- Settings has one **Android** page with the runtime picker, the keyboard toggles and the trackpad and mouse toggles; **Open picker** opens the runtime picker.
- Settings → General shows **Auto Update**, which the in-app updater honors, and still no Privacy section. Toggling Auto Update writes `auto_update` to settings.json.
- The welcome page lists recent projects in one list, under "Welcome back to Zdroid".

## 0.9.17 device checks

The 2026-10-28 audit (0.9.17) changed these paths without a device test:

- Bootstrap: open an existing project; terminal, git, LSP startup, a task, and an `npm install` of a package with native binaries (npm's `--libc=musl` now applies only under Bootstrap, not chroot).
- Runtime picker: switch runtimes and back with a customized `zd-runtime.toml` (settings must survive); a failed or offline Bootstrap install must leave an error in the picker; a saved External Termux selection must be replaceable from the picker.
- Shared-storage import: import a project, cancel one mid-copy, import the same project twice, and close the dialog during a copy (closing cancels it). No partial tree may open, and existing projects must stay untouched.
- Native AI: a fresh agent panel shows the API-key setup without Zed sign-in or trial prompts; the LLM Providers page lists no Ollama, LM Studio or llama.cpp; configure an OpenAI-compatible model, stream a reply with a tool call, run an inline edit, restart and check the key is still there.
- Keyboard-cover trackpad: quick two-finger taps still right-click, slow two-finger scrolls don't, and the cursor captures and releases in the main and extra windows.
- Files app (DocumentsProvider): search through a symlink loop in `~`; renaming or deleting the Zed root must be refused.

## Input review of 2026-10-26: open findings

An audit of `crates/gpui_android`'s input code found 22 problems. These are the ones still open when rechecked on 2026-10-27 (after #41). The first, captured trackpad and mouse events dropping the modifier keys, sat unrecorded until it showed up as "Ctrl+click on a link does nothing"; that is why the rest are listed here. The touch rewrite planned under "Post-2026-08 upstream-merge follow-ups" would fix the touch items (6 to 8) as a side effect.

**Mouse and trackpad**

1. **Double- and triple-click don't work.** `events/mouse.rs::button_up` always reports `click_count: 1`, and Zed reads the click count from the up event (`ClickEvent::click_count`), so double-clicking a project-panel file or a tab does nothing. Under pointer capture, `captured_pointer.rs` also sends 1 on every down. Fix: put the count `ClickTrackState` already tracks on the up event, and have `captured_pointer` call `next_click_count`. Tablet check: with the mouse, double-click a word in the editor and a file in the project panel.
2. **The wheel scrolls 1 line per notch on the uncaptured path and 3 under capture** (`events/mouse.rs::wheel_scroll` vs `MOUSE_WHEEL_LINES_PER_NOTCH` in `captured_pointer.rs`). Pick one.
3. **Captured two-finger tap bounds (fixed in code 2026-10-28; device check pending).** Right-click now requires exactly two fingers, completion within `TWO_FINGER_TAP_WINDOW`, and cumulative movement below the tap threshold. Verify quick taps still right-click and slow two-finger scrolls do not.
4. **Gpui is never told when the pointer leaves a window.** `HOVER_EXIT` is dropped (`events.rs`), the hover-status callback is stored but never called, and `is_hovered` returns false, so hover highlights can stay after the pointer leaves.
5. **S Pen buttons aren't mapped.** `button_from_state` ignores the stylus button bits, so the pen's side button doesn't right-click.

**Touch**

Fingers go to gpui's touch gestures since step 2 of the touch plan below, and step 4 deleted `touch.rs`'s finger state machine, so 6 and 7 are fixed and the finger long press in 8 fires from a timer. A long press outside the editor and the terminal, which claim it, still ends as a tap when the finger lifts.

6. ~~**Starting a scroll or long press can click.** Touch-down sends a mouse down at once, and the gesture ends with a `click_count: 0` mouse up, which gpui still treats as a click. Starting a scroll on a project-panel file can open it, and every scroll moves the editor's caret to where the finger landed. Fix: send the mouse down only once a tap is decided, as `TrackpadTouchState` already does.~~ Fixed: gpui's recognizer sends the mouse down only once a tap is decided.
7. ~~**A two-finger swipe right-clicks.** `touch.rs` fires the right-click as soon as a second finger lands within 800 ms and 250 px, so most two-finger swipes open the context menu. Fix: fire on lift if the fingers barely moved.~~ Fixed: `TouchFeed` fires on lift if the fingers barely moved.
8. **Trackpad-mode hold-to-drag fires only on the next move event**, and capture's drag lock releases only on the next finger down; a perfectly still finger never promotes to hold-to-drag. Losing pointer capture isn't reported to Rust either. (The finger long press, which had the same problem, now fires from gpui's timer.)

**Extra windows (Settings and other pop-ups)**

9. **Mouse and stylus input is treated as a finger** when not captured. `ExtraWindowActivity.forwardTouchEvent` sends no source, and `translate_extra_motion_event` sends every touch action to gpui's touch recognizer through `TouchFeed`, so a mouse click there is a tap and a mouse drag scrolls.

**Keyboard and IME**

10. **Hardware keyboards are read with a fixed US table** (`events/keyboard.rs::build_keystroke`, layout `"us"`, `DummyKeyboardMapper`). On a Turkish layout, ğ types `[`. AltGr characters, dead keys and numpad digits don't work. Fix: `AndroidApp::device_key_character_map(device_id)` (`get` with the meta state, `get_dead_char`) in the main window, and `KeyEvent.getUnicodeChar(metaState)` from Kotlin for extra windows. Tablet check: switch the keyboard cover's layout to Turkish and type ğüşıöç.
11. **The IME doesn't know where the caret is.** `update_ime_position` is empty and `ZdroidInputConnection` doesn't implement `requestCursorUpdates`, so floating keyboards and handwriting input can't position themselves.
12. **InputConnection contract gaps:** `setSelection`, `setComposingRegion` and `performContextMenuAction` aren't implemented, `deleteSurroundingText` also deletes the selection, and `newCursorPosition` is ignored.
13. **The keyboard still hides by polling** `input_handler.is_some()`, with a `text_for_range(0..1)` probe and a selection poll, instead of reacting to gpui's `text_input_state_changed`. (Text fields now show it through `request_virtual_keyboard`; see `crates/gpui_android/docs/ahead-of-upstream.md`.)
14. **IME callbacks call into the editor while holding a mutable borrow of the window state** (`ime.rs`, `platform.rs`). Nothing triggers it today, but a re-entrant call would panic.

**Code health**

15. Constants duplicated between `events/mouse.rs` and `captured_pointer.rs`; unchecked indexing in `touch.rs`; `lock().unwrap()` in `ime.rs`, `captured_pointer.rs` and `multi_window.rs`; `expect()` in `ime.rs`; stale comments across `events.rs`, `events/*.rs`, `touch.rs`, `window.rs` and `ime.rs`; dead `Finger` arms in `events/source.rs` and unused arguments in `touch.rs`.
16. Per-event work on the trackpad path: a view-tree search and array allocations per event in `MainActivity`, the relative delta computed twice, two relative-coordinate arrays per captured event (unused absolute-coordinate arrays were removed in the 2026-10-28 review), and a JNI call plus `runOnUiThread` per trackpad-mode move.

## Post-2026-08 upstream-merge follow-ups

The 2026-08-07 merge (d066a73609 -> 87e698fb6f, 1492 commits) landed with parallel APIs and deferred cleanups. None block shipping; all shrink future merge cost or unlock upstream features.

**Converge onto upstream's official mobile API surface.** Done (2026-10): the soft keyboard uses `show_soft_keyboard`/`hide_soft_keyboard` and `text_input_state_changed` (which also drives vim's key-vs-text routing), the platform reports `on_app_lifecycle`, `on_memory_warning` and window visibility, the keyboard and trackpad buttons are app-owned status bar items, and the windows implement `insets()`/`on_insets_changed`: each Activity pushes its system-bar, display-cutout and keyboard insets, every frame while the keyboard slides, and gpui lays out above the keyboard. Touch is done too (below). Left: `set_back_handler` and `set_text_input_configuration`, which wait on upstream, since nothing in Zed uses them yet; `crates/gpui_android/docs/ahead-of-upstream.md` says what to grep for at each sync. Touch was the big one; its plan, one pull request per step, each tested on the tablet:

1. gpui_android sends `PlatformInput::Touch` for finger input in the main and extra windows instead of `touch.rs`'s synthesized mouse and scroll events. gpui's recognizer (`crates/gpui/src/gestures.rs`) then provides taps with tap counts, pans with momentum, long presses, touch drags and `Window::last_input_was_touch`. The virtual trackpad (`TrackpadTouchState`) and the captured pointer stay as they are.
2. Upstream doesn't claim touch drags anywhere yet, so add claims (`prevent_default` on a started `TouchDragEvent`) to the scrollbar thumb, the editor scrollbar, the pane splitter and the dock resize handle, replacing the fork's `set_drag_active` calls. Add a `LongPressEvent` claim to the editor for word selection with drag-to-extend, and move the terminal's long-press menu onto it. These are small, platform-neutral edits that could go upstream.
3. gpui's recognizer has no two-finger tap: detect it in gpui_android before forwarding (cancel the touches, then send a secondary click).
4. Remove `set_drag_active`, `last_input_was_touch` and `last_platform_input_was_touch` from gpui and the finger state machine from `touch.rs`.

All four steps are done (2026-10). Step 1 landed behind an `android_input.legacy_touch` setting (on kept `touch.rs`'s state machine; off sent fingers to gpui's recognizer through `touch::TouchFeed`) and folded in step 3: `TouchFeed` turns a two-finger tap into a secondary click when the fingers lift, and a two-finger drag into one touch at the fingers' centroid. Step 2 claimed touch drags for the scrollbar thumb, the editor scrollbar, the pane splitter and the dock resize handle (the dock resizes through `Workspace::drag_dock_edge`, since gpui's drag and drop doesn't follow touches), long presses for the editor (word selection, drag to extend) and the terminal (selection, then the context menu on release), and made taps carry the keyboard's modifiers. Step 4 deleted the legacy path: `touch.rs`'s finger state machine, `set_drag_active`, `last_platform_input_was_touch` with `PlatformWindow::last_input_was_touch` behind it, and the `legacy_touch` setting; the touch-widened hit areas (splitter, dock handle, a scrollbar track press grabbing the thumb) read gpui's `Window::last_input_was_touch` instead.

**Finger drag and drop of tabs and project panel entries.** gpui's drag and drop (`on_drag`, `DragMoveEvent`) has no touch source, so a finger can't reorder tabs or move project panel entries; the release caveats list it. (The deleted legacy path started gpui's drag and drop from a long press, then a drag.) Native Android and iOS reorder with a long press, then a drag. Start gpui's drag from a `LongPressEvent` on Android.

`Application::run_embedded` still invokes `Platform::run`; it does not replace the Android event loop. `ThreadedDispatcher` is gated to tests and benchmarks in the tracked Zed release. Neither is a production replacement for the Android dispatcher at this tag. Recheck if upstream adds a production mobile dispatcher.

**Next Zed sync.** Zed v1.27.0-pre (2026-10-28) adds two `Platform` methods:

- `fn platform_info(&self) -> PlatformInfo`, required. `PlatformInfo { os: OperatingSystem, target: PlatformTarget }` lives in `crates/gpui/src/platform/platform_info.rs`; `OperatingSystem` has `Mac`, `Windows`, `Linux`, `FreeBsd`, `Ios`, `Android` and `Unknown`, and `PlatformTarget` has `Native` and `Web`. Neither carries a version, so `AndroidPlatform` returns `PlatformInfo { os: OperatingSystem::Android, target: PlatformTarget::Native }` and doesn't need `Build.VERSION`. The merge won't compile until it's there.
- `fn power_mode(&self) -> PowerMode` and `fn on_power_mode_change(&self, callback: Box<dyn FnMut()>)`, defaulting to `PowerMode::Unknown` and no callback. `PowerMode` is `Unknown`, `LowPower`, `Balanced` or `HighPower`; in `LowPower`, gpui caps animation frames at 30 a second. Implement both in `AndroidPlatform`: `LowPower` while `PowerManager.isPowerSaveMode()`, with `PowerManager.ACTION_POWER_SAVE_MODE_CHANGED` driving the callback. Otherwise return `Unknown`, as macOS does: the trait's docs reserve `Unknown` for a platform that can't tell the other modes apart, and Android reports only whether power saving is on.

**Audit new git CLI spawn sites.** git2 is gone; `crates/git/src/repository.rs` (+3.3k) and `project/src/git_store.rs` (+3.2k) now spawn `git` per operation through the zd-exec router (verified working on-device for status/trust). Sweep for spawn sites that assume desktop git semantics (pager, hooks, credential helpers) under the runtime's git.

**`sandbox` crate on Android.** New non-optional dep of the `zed` binary path; heavy backends are cfg-gated off android and it compiled clean, but its runtime fallback arm has never been exercised on-device beyond boot. Check what agent tool-calls hit it.

**`spawn_when_idle` support.** New defaulted `PlatformDispatcher` methods (`dispatch_on_main_thread_when_idle`, `idle_time_remaining`) are unimplemented on Android, so `BackgroundExecutor::spawn_when_idle` degrades to never-idle. Fine today; revisit if upstream starts scheduling real work through it.

**Spawn bridge consolidation (done 2026-10-28).** Android's async `util::command::Command` now converts the existing `gpui_util::new_std_command`, sharing environment-root and shebang routing with synchronous spawns.
