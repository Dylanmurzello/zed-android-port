# Choreographer-driven vsync

**Status:** Active
**Phase / Commit:** vsync callbacks from the first port; on-demand frames since Zdroid 0.8.6
**Files:** `crates/gpui_android/src/platform.rs` (`request_frame_callback`,
`frame_callback`), `crates/gpui_android/src/window.rs` (`frame_waker`,
`schedule_frame`)

## Problem

gpui paints a window when the platform asks it for a frame. The port first
polled the looper on a fixed 8ms timer, which drifted from the panel's
refresh. AChoreographer fixed the alignment: its callback runs on the main
thread's looper at each vsync, so frames start in step with the display.

The callback then re-posted itself on every vsync for the app's lifetime,
so the main thread woke at the panel's refresh rate, 120 times a second on
the reference hardware, even with nothing on screen changing.

## Constraint

Android only delivers a Choreographer callback that was posted for the next
vsync, and gpui has no frame to ask for while nothing is dirty. Something
has to post the callback again whenever gpui gets new work.

## Solution

gpui already has hooks for platforms that stop requesting frames while a
window is idle: it calls the window's `frame_waker` when the window becomes
dirty, and `schedule_frame` or the waker again after a frame while it still
has work, such as animations or next-frame callbacks. Its web backend uses
them to call `requestAnimationFrame` only on demand, and the Wayland
backend parks its frame loop the same way.

`AndroidWindow` implements both with `request_frame_callback`, which posts
one Choreographer callback unless one is already pending. `frame_callback`
no longer re-posts itself; it sets `FRAME_PENDING` for the run loop, which
refreshes every window. One callback serves the primary and extra windows.

gpui also keeps presenting for a second after fast input, so the panel
stays at its high refresh rate, but it only asks for frames while a window
is dirty. So the run loop keeps requesting frames for one second after
each input event (`FRAMES_AFTER_INPUT`), as the old continuous callback
did, and parks after that. This matters with Samsung's adaptive refresh,
which drops the panel to 60 or 30 Hz soon after an app stops rendering.

Repaints that gpui doesn't know about request a frame themselves: surface
attach after returning from the background, and GPU device-lost recovery.
The soft-keyboard tick sets `FRAME_PENDING` directly, as before.

## Why this works

An idle editor now wakes only for the run loop's 100ms poll timeout and real
events, instead of at the panel's refresh rate. While you type or scroll,
and for a second after, frames run on every vsync as before. Animations
still paint on the next vsync, because they dirty the window or schedule
a next-frame callback, which wakes the frame loop.

## Failure mode if regressed

- A change that alters what's on screen without notifying gpui leaves the
  window stale until the next input event. Request a frame from that code,
  as `attach_surface` does.
- `install_choreographer_callback` clears the pending flag on every
  `android_main` run. Without that, a callback posted on a recreated
  Activity's old looper would never fire, and frames would stop.
- If the callback re-posts itself again, idle wake-ups return to 120 a
  second. Zdroid's CPU use while idle shows it: `top` in Zdroid's own
  terminal, which runs as the app's user.

## See also

- [Render-pipeline perf polish](deferred-render-pipeline-perf.md), including
  the `frame_timing` stats, which are logged at debug level
