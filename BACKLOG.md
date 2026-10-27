# Zdroid backlog

Non-blocking UX / feature gaps to revisit after the current refactor lands. Tasks in the live task list are atomic and time-bound; this file is for "noted but not yet scoped" concerns.

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

- **Mouse scroll direction reversed**: Samsung One UI + Bluetooth mouse. Scroll works correctly everywhere else on the device but is inverted in Zdroid. Likely a sign flip in `gpui_android`'s `MotionEvent` → scroll-delta translation. Check the wheel-axis path specifically (touch-pad two-finger scroll may differ); One UI may report `AXIS_VSCROLL` with a sign that mainline Zed doesn't compensate for. **Still open.**
- ~~**Vim mode broken**~~ — **fixed in 29b29ddd63** (key events now route from `ExtraWindowActivity` to gpui via `nativeOnExtraKeyEvent` JNI bridge; the root cause was that any extra window's `dispatchKeyEvent` was never forwarded so `PlatformInput::KeyDown` never fired). Vim and any other editor in a non-Main window now accepts typing.
- ~~**Settings searchbar typing broken**~~ — **fixed in 29b29ddd63** (same key-routing fix, plus a tap-to-focus wrapper on the search bar with `stop_propagation` so taps don't bubble to `SettingsWindow`'s focus tracker which was immediately blurring the bar).

## Runtime picker: adapter install-state UX (deferred)

The onboarding runtime-picker section (and the Settings page entry) currently presents all three adapters — Kali chroot, Bootstrap, External Termux — as selectable equals. On a fresh install the user almost certainly has NONE of them installed:

- **Chroot** requires Magisk + the `zdroid-spawnd` Magisk module (separate flashable zip from GitHub releases) + a Kali NetHunter chroot rootfs at `/data/local/nhsystem/kali-arm64`. None of those are bundled in our APK.
- **Bootstrap** requires the bootstrap zip (~240 MB) extracted into `$PREFIX`. Auto-extracted from the APK asset today, but per task #17 we're moving to GitHub-release download. If the asset isn't present, "Bootstrap" doesn't work either.
- **External Termux** requires the Termux app installed on the device AND the user granting `com.termux.permission.RUN_COMMAND`. Plus our JNI Intent bridge (task #36) needs to land.

The picker doesn't surface any of this. A user tapping "Bootstrap" with no bootstrap installed sees: their selection saved, restart Zdroid, nothing works, no actionable feedback. Same for chroot without the Magisk module. Worst-case UX.

**Bridge needed:**

- Show an install-state badge per adapter (Installed / Missing / Partial).
- For Missing: link out to the install instructions (GitHub release page, F-Droid Magisk module link, Termux Play Store/F-Droid).
- For Partial (e.g., chroot module installed but rootfs path is empty): explain what's missing AND what to do.
- Consider whether to install silently (downloads bootstrap zip on selection) or surface as an opt-in download with progress.
- "Auto-install" vs "show options" — likely the right answer is BOTH, with auto-install gated behind explicit user consent because: bootstrap is 240 MB, chroot requires Magisk + root, external Termux needs a separate app install. Silent install is hostile to discoverability and bandwidth control.

The chroot adapter already has health-check hints in `RuntimeProvider::health_check()` — `HealthStatus::NotInstalled { hint }` with links to the spawnd release page. The picker UI just doesn't surface these yet. Wiring through to the inline section would give us the install-state-aware UX without inventing new infrastructure.

**Related tasks already in the live list:**

- #33 — wire settings UI for adapter selection
- #34 — first-launch onboarding modal (now landed as the onboarding page's runtime section, `runtime_picker::render_onboarding_section`, but install-state UX is still TBD)
- #36 — JNI Intent bridge for external Termux adapter
- #37 — bootstrap install/uninstall (GitHub releases download)

Resolve this AFTER the Termux-divestment refactor (tasks #74-80) lands. Once each adapter is a first-class peer with a proper install flow, the picker can surface that flow.

## Settings search bar input pipeline (deferred)

Tapping the settings search bar on Android doesn't bring up the soft keyboard and the Editor never receives input events. Probe instrumentation in `crates/settings_ui/src/settings_ui.rs` shows:

- Editor entity is created (we see `editor created` at boot)
- A `Blurred` event fires after the user taps, but no `Focused` event reaches the subscribe handler
- No `InputHandled` / `InputIgnored` / `Edited` events

So the Editor IS getting focused (Blurred can only fire after a Focused state), but the focus is being stolen immediately, AND the Android InputMethodManager (soft keyboard) is not being prompted. Two suspected layers:

1. **GPUI / gpui_android focus handling**: the on_click handler I added (focus the editor handle on bar click) appears to set focus but lose it instantly. Could be the welcome page or some other view stealing it back via focus rules.
2. **Android IME wiring**: even if focus stays on the Editor, the soft keyboard doesn't open. Zed-the-app needs to call `InputMethodManager.showSoftInput()` (Java/JNI) when an Editor gains focus on Android. This wiring is in gpui_android's platform layer and may be incomplete.

Diagnosing requires instrumenting gpui_android's input layer too, not just the editor subscription. Probably want JNI logs on `onWindowFocusChanged`, `dispatchKeyEvent`, and `InputMethodManager` calls. Out of scope for the Termux-divestment refactor; pick up after Phase 8.

Visible workaround for users today: there isn't one — search filter is effectively dead on Android. Settings remain navigable via the sidebar tree.

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

## MCP / context-server integration is absent on Android

**Repro:** add a `context_servers` block to Zdroid's `settings.json` (the syntax Zed uses upstream for stdio or HTTP MCP servers). Restart the app. Nothing happens. No server is spawned, no tools surface in any panel, no entry appears under Settings; the editor reads the JSON but no code path consumes the block. Adding the same block on desktop Zed works.

**Diagnosis:** the wiring is missing at three layers, in order:

1. **Boot skip.** `crates/gpui_android/examples/zed_android/src/lib.rs` around line 1003 explicitly lists `agent_ui` / `copilot` / `language_models` as out-of-scope on Android ("Skipped from production"). The agent panel is the entry point that registers MCP-related actions and observes the `context_servers` settings block; without `agent_ui::init` running, none of that fires.
2. **Cargo dep absent.** The example's `Cargo.toml` doesn't declare `agent_ui` or `context_server` as dependencies, so the upstream MCP transport implementations (`crates/context_server/src/transport/{stdio_transport,http}.rs`) aren't even linked into Zdroid's `.so`. There is no `McpClient`, no `ContextServerStore` instance in the process at any point.
3. **Subprocess spawn gap.** Even if 1 and 2 land as-is, the upstream stdio transport spawns the configured MCP server with `tokio::process::Command`, which on Android lands as a bionic-spawn from the editor process. That path does not see the active runtime adapter's `$PATH`, `$LD_LIBRARY_PATH`, or chroot scope, so any MCP server that ships as a Bootstrap-userland or chroot binary (`npx @modelcontextprotocol/server-filesystem`, `uvx mcp-server-time`, anything else realistic) fails to exec.

**Wiring needed:**

- Add `agent_ui` + `context_server` (and the transitive `language_models`, `assistant`, etc. that won't compile without it) to `crates/gpui_android/examples/zed_android/Cargo.toml`.
- Initialize `agent_ui::init` in `boot()` past the "AI skipped" comment band. Confirm the agent-panel + workspace action wiring actually fires on Android; the editor compiles agent surfaces today via cfg-gates to mocks, but full init likely surfaces side effects (LLM provider registration, settings observers, etc.) that we have not exercised.
- Route the stdio-transport spawn through `zdroid_runtime` so the configured MCP server inherits the active adapter's environment. Easiest path: replace the upstream `tokio::process::Command` invocation with a call that goes through our existing `zd-exec` wrapper (`crates/zdroid_runtime/src/bin/zd-exec.rs`); the wrapper already handles chroot/bootstrap/external Termux selection. Either patch upstream's transport with an Android cfg-gate, or wrap the `MCPCommand` config builder.
- Decide what to do with the LLM provider gate. MCP servers are useful even without a model talking to them (tool execution, prompt management), so a "BYO key, no provider configured" mode is viable. Alternatively gate MCP UI visibility behind a configured provider so the no-key state is not confusing.

**Why deferred:**

- The README and roadmap already declare AI panels out of scope. The MCP gap is downstream of that declaration; closing it pulls the entire `agent_ui` / `language_models` surface back in scope, which is a meaningfully larger commitment than just the MCP wiring.
- Daily-driving the editor today does not require MCP. Most of what people want from MCP on Android specifically (filesystem access, shell tools) is already reachable via the integrated terminal + the relevant runtime adapter, so users have a working workaround.
- The right time to do this is once we either (a) ship a real LLM-panel integration so MCP arrives with a sibling consumer, or (b) deliberately decide to surface MCP tools standalone (slash commands? command palette entries?) without the assistant.

**Scope estimate:** rough 2-3 days end-to-end. Cargo dep additions and `agent_ui::init` are short. The real time sink is debugging the side effects of bringing the agent surface online for the first time on Android (focus / window routing, settings store observers that assume desktop platforms, the LLM provider registration path on a platform that has historically been mock-only). Pair with [[project-zed-android-port]] phase planning when picking this up.

## Other deferred concerns

(add new sections here as they come up; keep each one self-contained with what / why / what's-needed-to-resolve)

## Input review of 2026-10-26: open findings

An audit of `crates/gpui_android`'s input code found 22 problems. These are the ones still open when rechecked on 2026-10-27 (after #41). The first, captured trackpad and mouse events dropping the modifier keys, sat unrecorded until it showed up as "Ctrl+click on a link does nothing"; that is why the rest are listed here. The touch rewrite planned under "Post-2026-08 upstream-merge follow-ups" would fix the touch items (6 to 8) as a side effect.

**Mouse and trackpad**

1. **Double- and triple-click don't work.** `events/mouse.rs::button_up` always reports `click_count: 1`, and Zed reads the click count from the up event (`ClickEvent::click_count`), so double-clicking a project-panel file or a tab does nothing. Under pointer capture, `captured_pointer.rs` also sends 1 on every down. Fix: put the count `ClickTrackState` already tracks on the up event, and have `captured_pointer` call `next_click_count`. Tablet check: with the mouse, double-click a word in the editor and a file in the project panel.
2. **The wheel scrolls 1 line per notch on the uncaptured path and 3 under capture** (`events/mouse.rs::wheel_scroll` vs `MOUSE_WHEEL_LINES_PER_NOTCH` in `captured_pointer.rs`). Pick one.
3. **Two-finger trackpad tap under capture has no time limit and checks motion per event.** `TWO_FINGER_TAP_WINDOW` in `captured_pointer.rs` is never read, and the 6 px check compares each move rather than the total, so a slow two-finger scroll can right-click when the fingers lift.
4. **Gpui is never told when the pointer leaves a window.** `HOVER_EXIT` is dropped (`events.rs`), the hover-status callback is stored but never called, and `is_hovered` returns false, so hover highlights can stay after the pointer leaves.
5. **S Pen buttons aren't mapped.** `button_from_state` ignores the stylus button bits, so the pen's side button doesn't right-click.

**Touch**

Since step 2 of the touch plan below, fingers go to gpui's touch gestures by default, which fixes 6 and 7 and fires the long press in 8 from a timer. A long press outside the editor and the terminal, which claim it, still ends as a tap when the finger lifts. The touch half of these items stays open only for `android_input.legacy_touch`, `touch.rs`'s finger state machine, until step 4 deletes it; trackpad-mode hold-to-drag, capture's drag lock and losing pointer capture in 8 are unchanged.

6. **Starting a scroll or long press can click.** Touch-down sends a mouse down at once, and the gesture ends with a `click_count: 0` mouse up, which gpui still treats as a click. Starting a scroll on a project-panel file can open it, and every scroll moves the editor's caret to where the finger landed. Fix: send the mouse down only once a tap is decided, as `TrackpadTouchState` already does.
7. **A two-finger swipe right-clicks.** `touch.rs` fires the right-click as soon as a second finger lands within 800 ms and 250 px, so most two-finger swipes open the context menu. Fix: fire on lift if the fingers barely moved.
8. **Long press and trackpad-mode hold-to-drag fire only on the next move event**, and capture's drag lock releases only on the next finger down; a perfectly still finger never long-presses. Losing pointer capture isn't reported to Rust either.

**Extra windows (Settings and other pop-ups)**

9. **Mouse and stylus input is treated as a finger** when not captured. `ExtraWindowActivity.forwardTouchEvent` sends no source or tool type, and `translate_extra_motion_event` sends every touch action to the touch state machine.

**Keyboard and IME**

10. **Hardware keyboards are read with a fixed US table** (`events/keyboard.rs::build_keystroke`, layout `"us"`, `DummyKeyboardMapper`). On a Turkish layout, ğ types `[`. AltGr characters, dead keys and numpad digits don't work. Fix: `AndroidApp::device_key_character_map(device_id)` (`get` with the meta state, `get_dead_char`) in the main window, and `KeyEvent.getUnicodeChar(metaState)` from Kotlin for extra windows. Tablet check: switch the keyboard cover's layout to Turkish and type ğüşıöç.
11. **The IME doesn't know where the caret is.** `update_ime_position` is empty and `ZdroidInputConnection` doesn't implement `requestCursorUpdates`, so floating keyboards and handwriting input can't position themselves.
12. **InputConnection contract gaps:** `setSelection`, `setComposingRegion` and `performContextMenuAction` aren't implemented, `deleteSurroundingText` also deletes the selection, and `newCursorPosition` is ignored.
13. **The keyboard still auto-shows by polling** `input_handler.is_some()`, with a `text_for_range(0..1)` probe and a selection poll, instead of reacting to gpui's `text_input_state_changed`.
14. **IME callbacks call into the editor while holding a mutable borrow of the window state** (`ime.rs`, `platform.rs`). Nothing triggers it today, but a re-entrant call would panic.

**Code health**

15. Constants duplicated between `events/mouse.rs` and `captured_pointer.rs`; unchecked indexing in `touch.rs`; `lock().unwrap()` in `ime.rs`, `captured_pointer.rs` and `multi_window.rs`; `expect()` in `ime.rs`; stale comments across `events.rs`, `events/*.rs`, `touch.rs`, `window.rs` and `ime.rs`; dead `Finger` arms in `events/source.rs` and unused arguments in `touch.rs`.
16. Per-event work on the trackpad path: a view-tree search and array allocations per event in `MainActivity`, the relative delta computed twice, four float arrays per captured event, and a JNI call plus `runOnUiThread` per trackpad-mode move.

## Post-2026-08 upstream-merge follow-ups

The 2026-08-07 merge (d066a73609 -> 87e698fb6f, 1492 commits) landed with parallel APIs and deferred cleanups. None block shipping; all shrink future merge cost or unlock upstream features.

**Converge onto upstream's official mobile API surface.** Done (2026-10): the soft keyboard uses `show_soft_keyboard`/`hide_soft_keyboard` and `text_input_state_changed` (which also drives vim's key-vs-text routing), the platform reports `on_app_lifecycle`, `on_memory_warning` and window visibility, and the keyboard and trackpad buttons are app-owned status bar items. Left: `insets()`/`on_insets_changed` (Kotlin would push the IME and system-bar insets), `set_back_handler`, and touch, which is the big one. Plan for touch, one pull request per step, each tested on the tablet:

1. gpui_android sends `PlatformInput::Touch` for finger input in the main and extra windows instead of `touch.rs`'s synthesized mouse and scroll events. gpui's recognizer (`crates/gpui/src/gestures.rs`) then provides taps with tap counts, pans with momentum, long presses, touch drags and `Window::last_input_was_touch`. The virtual trackpad (`TrackpadTouchState`) and the captured pointer stay as they are.
2. Upstream doesn't claim touch drags anywhere yet, so add claims (`prevent_default` on a started `TouchDragEvent`) to the scrollbar thumb, the editor scrollbar, the pane splitter and the dock resize handle, replacing the fork's `set_drag_active` calls. Add a `LongPressEvent` claim to the editor for word selection with drag-to-extend, and move the terminal's long-press menu onto it. These are small, platform-neutral edits that could go upstream.
3. gpui's recognizer has no two-finger tap: detect it in gpui_android before forwarding (cancel the touches, then send a secondary click), or propose it upstream.
4. Remove `set_drag_active`, `last_input_was_touch` and `last_platform_input_was_touch` from gpui and the finger state machine from `touch.rs`.

Step 1 landed behind `android_input.legacy_touch` (on keeps `touch.rs`'s state machine; off sends fingers to gpui's recognizer through `touch::TouchFeed`). It folds in step 3: `TouchFeed` turns a two-finger tap into a secondary click when the fingers lift, and a two-finger drag into one touch at the fingers' centroid.

Step 2 is done, and `legacy_touch` now defaults to off. Touch drags are claimed by the scrollbar thumb, the editor scrollbar, the pane splitter and the dock resize handle (the dock resizes through `Workspace::drag_dock_edge`, since gpui's drag and drop doesn't follow touches), long presses by the editor (word selection, drag to extend) and the terminal (selection, then the context menu on release), and taps carry the keyboard's modifiers. The `set_drag_active` calls stay until step 4; the terminal's double-click menu runs only on the legacy path. A finger can't drag tabs or project panel entries any more: on the legacy path a long press then a drag started gpui's drag and drop, and gpui's touch gestures have nothing for it yet.

Next is step 4: delete the legacy path, which is `touch.rs`'s finger state machine, `set_drag_active`, `last_platform_input_was_touch` (and `PlatformWindow::last_input_was_touch` behind it) and the `legacy_touch` setting. The touch-widened hit areas (splitter, dock handle, a scrollbar track press grabbing the thumb) then read gpui's `Window::last_input_was_touch` instead.

Also evaluate `Application::run_embedded` as a replacement for the custom android run-loop shim, and upstream's `ThreadedDispatcher` as a replacement for the hand-rolled calloop worker pool in `gpui_android/src/dispatcher.rs`.

**Accessibility tree.** `PlatformWindow::a11y_init`/`a11y_tree_update` defaults mean Android currently reports nothing to AccessKit. Wire it to Android's accessibility APIs (upstream: `crates/gpui/src/window/a11y.rs`).

**Audit new git CLI spawn sites.** git2 is gone; `crates/git/src/repository.rs` (+3.3k) and `project/src/git_store.rs` (+3.2k) now spawn `git` per operation through the zd-exec router (verified working on-device for status/trust). Sweep for spawn sites that assume desktop git semantics (pager, hooks, credential helpers) under the runtime's git.

**`sandbox` crate on Android.** New non-optional dep of the `zed` binary path; heavy backends are cfg-gated off android and it compiled clean, but its runtime fallback arm has never been exercised on-device beyond boot. Check what agent tool-calls hit it.

**`spawn_when_idle` support.** New defaulted `PlatformDispatcher` methods (`dispatch_on_main_thread_when_idle`, `idle_time_remaining`) are unimplemented on Android, so `BackgroundExecutor::spawn_when_idle` degrades to never-idle. Fine today; revisit if upstream starts scheduling real work through it.

**`util::command` bridge doc.** The zd-exec spawn bridge (`new_std_command` android arm, env-root routing, shebang rewrite) moved into `crates/gpui_util` so upstream's `pub use gpui_util::new_std_command` re-export carries the rewrite for sync spawn sites too. Update the workarounds index entry that still points at util/command.rs internals.
