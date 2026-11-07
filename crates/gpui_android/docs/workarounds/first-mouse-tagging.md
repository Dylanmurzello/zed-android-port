# Stop tagging Android MouseDowns as first_mouse

**Status:** Active
**Phase / Commit:** `bcfc976aa9` — Stop tagging Android MouseDowns as first_mouse
**Files:** `crates/gpui_android/src/events/mouse.rs` (`button_down`)

## Problem

Project panel needed two clicks on Android to focus + act. Many `on_click`
handlers behaved as if every touch was a "click to focus the window first".

## Constraint

GPUI's `MouseDownEvent` has a `first_mouse: bool` field that originates
from macOS's window-focus model: the first click after window activation is
the one that brings the app forward, and listeners can choose to bail out
on that click so the user's first click "just" focuses without triggering
an action. ProjectPanel.on_click does exactly this — first click focuses
the panel, second click acts on the entry.

`events.rs` was hardcoding `first_mouse: true` for every Android touch.
Result: every tap looked like a focus-the-window click, listeners that
respected `first_mouse` no-op'd on every touch.

## Solution

Always set `first_mouse: false` on Android. Android has no window-focus
concept the way macOS does; touches don't need to "wake" the window. Every
tap is a real click.

## Why this works

- Activity has focus when visible; touch events delivered to it are real
  user actions.
- macOS's first_mouse semantics are tied to NSWindow's
  `acceptsFirstMouse:`, which has no Android counterpart. Always setting
  `false` matches Android's actual semantics.

## Failure mode if regressed

- Project panel needs two clicks per entry. File finder needs two clicks.
  Tab close button needs two clicks. Symptom is "everything feels
  half-broken" rather than a hard error.

## See also

- [two-finger-rightclick.md](two-finger-rightclick.md) — related touch
  input quirks
