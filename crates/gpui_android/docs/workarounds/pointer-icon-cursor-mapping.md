# Map gpui CursorStyle to Android PointerIcon via JNI

**Status:** Active
**Phase / Commit:** `aa8521e6f6` — Map gpui CursorStyle to Android PointerIcon via JNI
**Files:** `crates/gpui_android/src/cursor.rs`, `ZdroidWindow.kt` (`setCursorStyle`)

## Problem

A connected mouse / trackpad never showed the resize-handle, I-beam,
pointing-hand, etc. cursors that desktop Zed renders on hover. Cursor
stayed as the default arrow regardless of what gpui asked for.

## Constraint

`AndroidPlatform::set_cursor_style` was a no-op. gpui's
`reset_cursor_style` fires on every input event (every mousemove, every
hover transition), so any naive bridging would JNI-hop on every event —
hundreds of times per second under continuous mouse motion.

## Solution

Bridge through JNI to call `View.setPointerIcon(PointerIcon.getSystemIcon(...))`
on the activity's decor view. Two pieces:

1. JNI bridge in MainActivity that takes a `cursor_kind: int` and resolves
   it to the matching `PointerIcon.TYPE_*` constant.
2. Thread-local cache on the Rust side that remembers the last cursor style
   set on this thread. If `set_cursor_style` is called with the same style,
   skip the JNI call. Avoids the round-trip on every input event.

## Why this works

- `View.setPointerIcon` is the canonical Android API for changing pointer
  appearance. System icons cover all the common gpui CursorStyle variants.
- Thread-local cache is correct because cursor state is per-window and
  gpui's `reset_cursor_style` always runs on the same thread.

## Failure mode if regressed

- Stub method reintroduced → cursor stays as default arrow regardless of
  hover. UX regression but not a hard error.
- Cache key wrong / cleared too often → JNI thrashing on every mousemove,
  observable as input-thread hot-spot in profiling.

## See also

- [assetsource-icons.md](assetsource-icons.md) — same shape of "what was
  a no-op stub now actually works"
