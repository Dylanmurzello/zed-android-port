# Extra-window input parity (hover, scroll, secondary)

**Status:** Active
**Phase / Commit:** L9 follow-up (input UX polish)
**Files:**
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/ExtraWindowActivity.kt`
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/NativeBridge.kt`
- `crates/gpui_android/src/events.rs` (`translate_motion`)
- `crates/gpui_android/src/multi_window.rs` (`Java_..._nativeOnMotionEvent` + `ExtraWindowEvent::Motion`)
- `crates/gpui_android/src/platform.rs` (Motion drain arm)

## Symptom

In secondary gpui windows hosted by `ExtraWindowActivity` (Settings,
Keymap, Themes, Extensions, Image / Markdown / SVG previews when they
get popped out, etc.), three categories of mouse / trackpad input
silently no-op while the same input works in the primary `MainActivity`
window:

- **Hover-to-show scrollbar** — moving the mouse pointer (no button
  pressed) into a scrollable region doesn't fade the thumb in. The
  scrollbar only appears once the user clicks somewhere, after which
  the scrollbar autohide state machine treats the click as a hover and
  shows it for the standard 1500ms.
- **Mouse wheel + trackpad two-finger scroll** — both produce no scroll
  on the secondary window. Same gesture works in the editor / terminal
  / project panel inside MainActivity.
- **Right-click via mouse / trackpad** — no context menu, no
  `on_secondary_mouse_down` handlers fire.

## Root cause

Two-layer gap:

1. **`ExtraWindowActivity.kt`'s `SurfaceView` only installed
   `setOnTouchListener`.** Hover events (`ACTION_HOVER_*`) come through
   `setOnHoverListener`; scroll-wheel + button events come through
   `setOnGenericMotionListener`. Neither was wired, so the system never
   delivered them to our forwarder.

2. **The extra windows' translator was deliberately a stub**, handling only
   `ACTION_DOWN`, `ACTION_UP` and `ACTION_MOVE`.

## Fix

### 1. Listeners on `ExtraWindowActivity.kt`'s `SurfaceView`

```kotlin
setOnTouchListener { _, event -> forwardTouchEvent(id, event); true }
setOnHoverListener { _, event -> forwardTouchEvent(id, event); true }
setOnGenericMotionListener { _, event -> forwardTouchEvent(id, event); true }
```

All three forward to the same `forwardTouchEvent` JNI bridge — the Rust
side dispatches by `event.actionMasked` so a single bridge serves all
sources. Returning `true` from each listener claims the event so the OS
chrome (drag handle, freeform resize edges) doesn't try to re-route it.

### 2. Bridge two new floats through the JNI signature

`ACTION_SCROLL` carries its delta in `event.getAxisValue(AXIS_VSCROLL)`
/ `AXIS_HSCROLL` — `getX/Y` return the pointer position, NOT the scroll
amount. The Kotlin side reads both axes unconditionally; they're zero
on non-scroll events. Rust-side `Java_..._nativeOnMotionEvent` and
`ExtraWindowEvent::Motion` carry them through.

### 3. One translator

The Kotlin side marshals the Java `MotionEvent`'s fields, because a `jobject`
can't be shared with the gpui main thread, and Rust builds the same
`events::Motion` from them that the main window builds from
`android_activity::input::MotionEvent`. Both go through `translate_motion`, so an
extra window treats a mouse, a stylus and a finger as the main window does.

## Failure mode if regressed

- New listener removed from `ExtraWindowActivity.kt` → hover / scroll /
  buttons silently no-op in extra windows again. Editor / terminal in
  MainActivity stay working (a different input path), masking the bug.
- `event.getAxisValue(AXIS_VSCROLL)` not extracted on the Kotlin side
  → mouse wheel returns zero delta, scroll fires but moves nothing.
  Subtle: action arm runs, no panic, just a no-op.
- New extern fn signature in `nativeOnMotionEvent` not matched on
  the Kotlin `external fun nativeOnMotionEvent(...)` side → JNI
  signature mismatch crash on first touch event in any extra window.
  `UnsatisfiedLinkError` at the call site, not at load time.

## See also

- [activate-extra-activity-move-to-front.md](activate-extra-activity-move-to-front.md)
  — how secondary windows get focused for `cx.windows()` dedup
