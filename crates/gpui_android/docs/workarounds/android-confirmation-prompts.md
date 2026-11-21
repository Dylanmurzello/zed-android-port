# Confirmation prompts fit Android windows

**Status:** Active
**Phase / Commit:** Post-0.4.3 GitHub round (2026-11-15), taken over from PR #23 by Japhet17429475581
**Files:** `crates/ui_prompt/src/ui_prompt.rs`

## Problem

Every confirmation on Android (the runtime reset question, Save / Don't Save / Cancel, the updater's notice) is drawn by Zed's own prompt renderer at its desktop size: 20 rem wide, 22 px buttons, no scrolling. On a tablet the detail text wrapped into a narrow column and the reset question's detail was clipped; in a narrow freeform window the dialog ran past the surface; a prompt listing many unsaved files had buttons below the fold; and the buttons were too small for a finger.

## Constraint

`AndroidWindow::prompt` returns `None` because Android has no native dialog the backend could use, so gpui falls back to `ui_prompt`'s `ZedPromptRenderer`. The app installs that builder right after `workspace::init` and keeps it installed when `use_system_prompts` is on. A platform dialog instead would lose Zed's theme and the Markdown in the detail text, and the renderer is an upstream file, so the change has to stay small and gated.

## Solution

Behind `cfg(target_os = "android")`, the dialog gets an id, a width of the viewport minus 32 px capped at 36 rem, a maximum height of the viewport minus 32 px, and `overflow_y_scroll`; the message, the detail and the button group are `flex_shrink_0` so the container scrolls instead of squeezing them; the buttons are rendered at 48 px with a wider gap. Desktop keeps its layout. The caller's button order, the response indices, the arrow-key tint and the focus restore are untouched: `on_click` is attached before the button is rendered.

## Why this works

gpui lays the prompt out at window size and the renderer centers the dialog in an `inset_0` overlay. Android windows have server-side decorations, so the overlay equals `window.viewport_size()`, which already excludes the soft keyboard. The scroll offset lives in element state under the `zed-prompt` id, so a scrolled dialog keeps its position across frames and resets when the prompt is dismissed.

## Failure mode if regressed

The reset question's detail is cut off again and the Save prompt for many files hides its buttons; a lost `flex_shrink_0` makes the message shrink to nothing instead of scrolling; a lost gate changes the desktop, iOS and web dialogs.

## See also

- [android-cfg-gates.md](android-cfg-gates.md) lists every gate in upstream files, this one included.
- [path-prompts-use-zed-picker.md](path-prompts-use-zed-picker.md) for the other dialog Android has to draw itself.
