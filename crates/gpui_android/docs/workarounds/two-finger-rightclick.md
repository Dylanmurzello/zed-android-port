# Two-finger tap → right click

**Status:** Active
**Phase / Commit:** `3193130a4b` — Switch Android right-click to two-finger tap
(supersedes earlier `d4998d1d19` long-press-as-right-click)
**Files:** `crates/gpui_android/src/touch.rs` (`TouchState` for the legacy path,
`TouchFeed` for gpui's gestures)

## Problem

Touchscreens have no right mouse button. The first attempt was long-press
→ right click, but it interfered with text selection in the terminal (and
any element where the user wanted to select text first and then invoke a
context menu): the synthesized `Down(Left)` cleared the prior selection
before the synthesized `Down(Right)` could show the menu, so the user got
the menu but their selection was gone.

## Constraint

We need a touchscreen gesture that:

- Doesn't interfere with text selection (so not long-press)
- Doesn't interfere with normal tap (so not single-tap)
- Maps cleanly to Android's `MotionEvent` model so we can detect at
  `Down`/`Up` boundaries
- Coexists with USB mouse / trackpad right-click (which arrives via
  `MotionEvent`'s `button_state` with `BUTTON_SECONDARY` set)

VNC / tablet-OS convention: **two-finger tap = right-click on the primary
finger's position.**

## Solution

Finger input takes one of two paths, chosen by `android_input.legacy_touch`
when a gesture's first finger lands.

**Legacy path** (`legacy_touch` on, the default; `TouchState` in `touch.rs`).
Resolved when the second finger lands:

- Single finger: emit `MouseDown(Left)` at once.
- Second finger lands within **800 ms** of the first and within **250 px** of
  where the first landed (`TWO_FINGER_TAP_WINDOW`,
  `TWO_FINGER_TAP_SLOP_PX`): release the pending `Left` with `click_count: 0`
  and emit `Down(Right)` + `Up(Right)` at the first finger's landing point.
  Also fires from a long-press selection, whatever the timing, so a second
  finger opens the context menu on the selected word.
- Otherwise: a two-finger scroll.

Most two-finger swipes start within those limits, so they right-click.

**gpui gestures path** (`legacy_touch` off; `TouchFeed` in `touch.rs`).
gpui's recognizer follows one touch at a time, so `TouchFeed` holds
two-finger gestures back and resolves them itself:

- A second finger lands while the first is still within the touch slop
  (Android's `ViewConfiguration.getScaledTouchSlop()`): cancel the first
  finger's touch and send nothing more for now.
- Neither finger moves more than twice the touch slop, and the last one
  lifts within **500 ms** of the second landing: emit `Down(Right)` +
  `Up(Right)` at the first finger's landing point, with the modifier keys
  held at the time.
- The fingers move further: one touch at their centroid starts, so gpui
  pans it with momentum. It ends when a finger lifts.
- A third finger cancels the gesture.
- A second finger landing after the first already left the slop is passed
  on as a touch of its own, which gpui ignores while it follows the first.

For trackpad / USB mouse right-click: Android resolves the gesture for us
and sends `MotionEvent` with `BUTTON_SECONDARY` in `button_state`. Skip the
touch-style detection, emit `Down(Right)`/`Up(Right)` directly.

## Why this works

- Two-finger tap is a deliberate user gesture that wouldn't otherwise be
  used for single-touch operations.
- On the gpui gestures path, deciding on lift tells a tap from a swipe: a
  swipe moves the fingers past twice the touch slop before they lift.
- Trackpad / mouse path is handled by Android's gesture resolver — we
  just respect what `button_state` says.

## Failure mode if regressed

- Long-press reintroduced as right-click → text selection broken in
  terminal and elsewhere (the old bug).
- Slop too tight → two genuine two-finger taps slightly offset get
  rejected as separate single-tap gestures.
- Slop too loose → two-finger scroll gets misclassified as right-click.
- First finger's touch not cancelled on the gpui gestures path → gpui
  taps or pans with the first finger as well as the right-click.

## See also

- [first-mouse-tagging.md](first-mouse-tagging.md)
- [jvm-clipboard-stack-overflow.md](jvm-clipboard-stack-overflow.md)
