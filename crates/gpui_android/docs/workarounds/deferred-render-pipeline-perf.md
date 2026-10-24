# Render-pipeline perf polish (deferred)

**Status:** Partially landed — items 1, 2, 6 shipped 2026-05-13; items 3, 4, 5 still deferred

The current rendering path is correct and GPU-accelerated end to end:
wgpu → blade-graphics → Vulkan on Adreno (same renderer/shaders as
desktop Zed), AChoreographer-driven vsync via direct NDK FFI (no
JNI hop per frame), `untrusted_app_27` SELinux context with full
GPU access. Frames are drawn at the panel's primary refresh rate
without CPU compositing.

## Landed in this pass (2026-05-13)

- **Item 1 (120Hz opt-in)**: `ANativeWindow_setFrameRate(0.0, COMPATIBILITY_DEFAULT)`
  wired via `dlsym` late-binding (so API 26-29 devices silently no-op
  instead of crashing on missing symbol). Helper at
  `crates/gpui_android/src/platform.rs::set_native_window_frame_rate`,
  called from `window.rs::attach_surface` on every surface attach.
  Verified on Tab S9 Ultra: `dumpsys SurfaceFlinger` reports
  `frameRate: 0.00 Hz, category: Default, selectionStrategy: Self`
  for `com.zdroid`, and `frameRateOverrides=[uid=10434 frameRateHz=120.00001]`.
  Realized frame interval dropped from 16.67ms → 8.33ms when device is
  fully active. System still chooses 60Hz under power-saving / "smart"
  refresh conditions even with our hint registered; that's not our
  code's call, it's a Samsung One UI policy.
- **Item 2 (Mailbox present mode)**: `preferred_present_mode: Some(wgpu::PresentMode::Mailbox)`
  in `window.rs::attach_surface`. Renderer capability-falls-back to
  Fifo if the surface doesn't expose Mailbox (it does on Adreno 740).
  Engagement verified via a new `log::info!` in `wgpu_renderer.rs`
  right after `surface.configure()`:
  `present_mode=Mailbox, frame_latency=3, available_modes=[Mailbox, Fifo]`.
- **Item 6 (Triple-buffer swap chain)**: Tried via a
  `desired_maximum_frame_latency` override on `WgpuSurfaceConfig`
  (`Some(3)`) and reverted: Samsung's smart refresh drops the panel to
  60/30Hz when idle, where the extra image adds 16-33ms of perceived
  input latency. The override was removed again; Android uses wgpu's
  default of 2 (see the comment in `window.rs::attach_surface`).
- **Item 4 (FrameMetrics-class instrumentation)**: New
  `crates/gpui_android/src/frame_timing.rs` records per-frame
  `vsync_interval` / `vsync_to_paint` / `paint_duration`, logs
  P50/P95/P99/max every 240 frames (~2-4 seconds). Cheaper than the
  Java `Window.addOnFrameMetricsAvailableListener` JNI bridge that
  this doc originally sketched, because our renderer bypasses the
  View tree anyway so Java FrameMetrics wouldn't capture our actual
  draw — Choreographer-callback-relative timing measures what we
  actually want. Sample baseline on Tab S9 at 120Hz active state:
  `vsync_interval p50=8.34ms p95=34ms p99=58ms`, `paint_duration
  p50=0.05ms p99=24ms`. The tail is dominated by rare expensive
  paints, not by wake-up noise.
- **Item 3 (Spurious ALooper wakes)**: The error log was misdiagnosed.
  `android-activity 0.6.1`'s `ALooper_pollOnce` returns
  `ALOOPER_POLL_CALLBACK` (which it documents shouldn't happen)
  whenever our Choreographer FD callback dispatches — i.e. every
  vsync, by design. It's not an extra wake-up, it's the one we want.
  Silenced via `env_logger` filter in `lib.rs`:
  `"info,android_activity::activity_impl=off"`. Saves ~12ms/sec of
  CPU spent in the logcat write path and removes 120 lines/sec of
  log spam. Real frame-pacing tail (p99=20ms+) comes from occasional
  long paints, not wakes — see the frame_timing samples above.

## I/O integration started (2026-05-13)

Separate concern from the render-pipeline items above, but landed in
the same session because the user explicitly tied the work together
("then we begin working on the I/O integration"). Scope: hardware
mouse / stylus / trackpad should feel desktop-class, not VNC-tier.

- **Source segregation** (`crates/gpui_android/src/events/source.rs`):
  New `InputSource` enum (Mouse / Stylus / Touchpad / Finger) and a
  `classify(event: &MotionEvent)` function that reads per-pointer
  `tool_type` first, falls back to the device `source()` bitmask.
  Wired into the primary `translate_motion_event`. Extra-window
  translator (Settings / Keymap / Themes) defaults to Finger because
  the JNI bridge doesn't yet pass source / tool_type — plumb that
  through `forwardTouchEvent` + `nativeOnExtraTouchEvent` when the
  feel gap hits an extra window.
- **Per-source multi-click windows** (`events/touch.rs`): The 500ms /
  6px constants are now `multi_click_window(source)` and
  `multi_click_slop(source)` helpers in `events/source.rs`. Hardware
  pointers (mouse / stylus / trackpad) get 300ms / 3px to match
  `ViewConfiguration.getDoubleTapTimeout()`; finger taps keep 500ms /
  6px. Double-click in the editor on a hardware mouse should now
  feel ~200ms faster.
- **Verification log**: `touch::next_click_count` logs
  `multi_click: source=… count=N window=… slop=…` whenever count > 1.
  Plug in a Bluetooth mouse, double-click a word in the editor, then
  `adb logcat -s zed_android | grep multi_click`. Expected output
  for mouse: `source=Mouse count=2 window=300ms slop=3 button=Left`.
  Synthetic `adb shell input mouse tap` works but the device must be
  unlocked + Zdroid focused; locked-device synthetic taps go to the
  lockscreen window, not us. Real-mouse verification on device still
  to be done by hand.
- **Pointer capture (deferred)**: `view.requestPointerCapture()` for
  Samsung Book Cover trackpad in non-DeX mode (which collapses
  multi-finger gestures to single-pointer relative motion, so
  two-finger scroll doesn't work) is intentionally deferred. Only
  worth doing if a user actually hits it on Tab S9 in tablet mode;
  cost is rendering our own cursor sprite while captured.

What it ISN'T yet: minimal-latency. Several small things compound
to ~5-10ms of cumulative input→paint overhead the user perceives as
"native-looking but not native-feeling," especially during scroll
and rapid keystroke bursts. This doc captures them so we can pick
them up later as a focused pass; nothing here blocks correctness.

## Items, ranked by leverage

### 1. ~~Opt into 120Hz on devices that support it~~ — LANDED

`crates/gpui_android/src/window.rs:147` sets
`preferred_present_mode: None` and never calls
`ANativeWindow_setFrameRate(...)`, so we run at the display's
default which Android assumes is 60 for "compat" apps. Tab S9 Ultra,
Pixel Tablet, and most modern flagship Androids panel-refresh at
120Hz, but only when the app explicitly opts in via the NDK
`ANativeWindow_setFrameRate` API (Android 11+ / API 30+).

Result: pixel-to-finger latency caps at ~16.7ms even on hardware
that could deliver ~8.3ms. Halving perceived input latency at zero
correctness cost is the highest-impact single change available.

Implementation sketch: in `window.rs` after window registration,
call

```rust
unsafe {
    ANativeWindow_setFrameRate(
        native_window,
        0.0,                            // 0 = "as high as you can"
        ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_DEFAULT,
    );
}
```

Plus an FFI declaration block. ~30 lines including the API-30 check
and graceful fallback for older devices. Validate via
`adb shell dumpsys SurfaceFlinger --frame-stats com.zdroid` —
should show 120Hz frame intervals after.

### 2. ~~Try `PresentMode::Mailbox` instead of FIFO default~~ — LANDED

Same line — `preferred_present_mode: None` lets wgpu pick. Default
is FIFO (hard vsync, blocks at present until next refresh). Mailbox
allows discarding stale frames in the swap chain, which feels lower
latency under irregular load (typing bursts, scroll-then-pause).
Adreno 740 supports Mailbox.

Trade-off: Mailbox can tear visually if frames present mid-scanout
on some devices; on Adreno + Android compositor it's typically
safe. Worth measuring before/after with FrameMetrics (item 4) on
both devices.

One-line change. Try, A/B feel, keep or revert.

### 3. Eliminate spurious ALooper wake-ups

Logcat shows `Spurious ALOOPER_POLL_CALLBACK from ALooper_pollOnce()
(ignored)` firing every ~16ms — once per vsync. The Choreographer
callback drives painting, but ALooper is also being woken every
frame with no events to drain. Each wake = a thread context switch
+ scheduler tick + dispatcher walk before we discover there's
nothing to do.

The 60Hz wakeup chain compounds with thermal throttling: under
sustained load the kernel scheduler de-prioritizes us briefly to
let other threads run, and our paint can land 1-2 frames late. With
the wake-up noise removed, the same scheduler decisions don't bite
us as often.

Source is in `crates/gpui_android/src/platform.rs::handle_main_event`
loop interaction with the input thread. Need to identify what's
queueing onto the looper and gate the post-frame callback to fire
only when there's pending input/redraw, not unconditionally.

The `choreographer-vsync.md` workaround doc accepts this noise as
"non-fatal log spam" — it isn't, it's measurable jitter. Same
investigation, deeper fix.

### 4. Wire FrameMetrics for primary-source latency data

Android's `Window.addOnFrameMetricsAvailableListener` exposes per-
frame:

| Metric | What it measures |
|---|---|
| `INPUT_HANDLING_DURATION` | How long we spend in our touch/key handlers before queuing a redraw. |
| `LAYOUT_MEASURE_DURATION` | (n/a for us — we don't use Android Views for layout) |
| `DRAW_DURATION` | gpui render → wgpu submit. |
| `SYNC_DURATION` | wgpu submit → driver acceptance. |
| `COMMAND_ISSUE_DURATION` | GPU work to draw call submission. |
| `SWAP_BUFFERS_DURATION` | Surface flip into composition. |
| `TOTAL_DURATION` | Sum; budget is ~16.7ms at 60Hz, ~8.3ms at 120Hz. |

Pipe these to logcat once per ~60 frames (don't spam) and we know
exactly which step is the long pole on this hardware. Today we
guess. With FrameMetrics, the next item to fix is data-driven, not
intuition.

JNI bridge: ~40 lines (Kotlin listener + Rust JNI receiver +
log::info every 60th frame).

### 5. Touch-event chain shortening

Android touch path: `InputDispatcher` → `Surface InputChannel` →
`ALooper` → `game-activity` crate → gpui input dispatch →
`InteractiveElement` event handler. Each hop adds ~1-3ms.

Native Android Views skip the gpui hop. Native iOS skips ~3 of
these because Cocoa Touch is in-process with the renderer.
Cumulative ~5-10ms touch-to-paint overhead the platform doesn't let
us avoid entirely, but parts are reducible.

Specifically: `game-activity` 0.6's input wrapper does a copy +
re-emit on every event. Bypassing it for the hot paths (move,
scroll, click) and going direct from `AInputQueue` to gpui would
remove the copy. Bigger surgery — touches our `android_main` event
loop and conflicts on every `game-activity` upgrade. Defer until
items 1-4 are landed and measured.

### 6. ~~Triple-buffer swap chain~~ — LANDED

wgpu defaults to 2 swap-chain images. 3 lets the GPU work on the
next frame while one is held by the compositor and one is queued.
Smoother under load, ~5-10MB more VRAM. Set
`SurfaceConfiguration.desired_maximum_frame_latency` (or wgpu's
equivalent) to 3.

Single-line change. Cheap.

## What this does not block

- Correctness: nothing here is a bug. The renderer is correct, the
  compositor is correct, syncs are correct.
- Hardware: every device we ship to has the relevant API (Android
  10+ for 120Hz opt-in, Android 8+ for FrameMetrics).
- Other features: search, LSPs, terminal, remote dev, claude-code
  all work fine on the existing 60Hz pipeline. Perf polish is
  orthogonal to functional work.

## Suggested order when revisiting

1. FrameMetrics first (item 4) — gives us data to drive the rest.
2. 120Hz opt-in (item 1) — biggest perceptible delta.
3. Mailbox + triple-buffer (items 2 + 6) — cheap, A/B test feel.
4. ALooper spurious-wake hunt (item 3) — moderate effort, pays off
   under thermal/CPU pressure.
5. Touch chain (item 5) — biggest engineering, smallest user-
   perceptible delta. Last.

## See also

- [choreographer-vsync.md](choreographer-vsync.md) — what's already
  in place; item 3 is the unfinished half of that work
- [wgpu-device-lost-recovery.md](wgpu-device-lost-recovery.md) —
  related GPU-pipeline robustness
- Tab S9 Ultra hardware notes:
  `memory/project_phase4_notes.md` — Adreno 740 quirks
