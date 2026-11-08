# Confirmation prompts fit Android windows

**Status:** Active
**Files:** `crates/ui_prompt/src/ui_prompt.rs`

## Problem

Confirmation prompts use the desktop renderer's fixed 20-rem width. On a
tablet this leaves very little room for messages and file paths; in a narrow
freeform window the dialog can extend past the surface. Long messages and
details have no scrollable container, and the default buttons are only
22 logical pixels high.

## Constraint

`AndroidWindow::prompt` returns `None` because the backend has no native
confirmation dialog. The Android application already calls `ui_prompt::init`
immediately after `workspace::init`, and `process_settings` keeps Zed's prompt
builder installed on Android even when `use_system_prompts` is enabled.
Installing that builder again does not change the layout. Replacing it with an
Android system dialog would also lose Zed's theme and Markdown rendering.

## Solution

Adapt the existing Zed renderer behind Android cfg gates:

- Use the viewport width minus 32 logical pixels, capped at 36 rem, and limit
  the dialog height to the viewport height minus 32 logical pixels. Render-time
  sizing follows rotation, keyboard resizing and freeform window resizing.
- Scroll the dialog vertically. Its message, detail and button group do not
  shrink, so long content and all actions remain reachable even in a short
  window. The existing padding, Markdown styles and themed surface are reused.
- Keep full-width buttons in caller order, with 48-logical-pixel heights and
  larger gaps. Clicks still emit the original action index through
  `PromptHandle`, which dismisses the prompt and restores the previous focus.
- Preserve the explicit `PromptButton::Cancel` index before labels are
  converted to strings. Escape uses that role even for a label such as
  "Keep Editing", with the existing "Cancel" label lookup as a fallback.
  Prompts without a cancel action are not dismissed by Escape.

The platform backend and workspace initialization need no changes. Desktop,
iOS and web retain their existing layout and cancel behavior.

## Manual verification

On an Android device, open a file, edit it and close it to trigger the
save/discard/cancel prompt. Check the following with both light and dark themes:

1. On a tablet or a wide freeform window, the prompt grows beyond its old
   20-rem width while retaining a readable maximum width.
2. In a narrow window, the prompt keeps 16 logical pixels of space on each
   side. Rotate or resize while it is open and check that it still fits.
3. With a long message or many unsaved filenames, scroll through the content
   to every button. Repeat in landscape with the soft keyboard visible.
4. Tap each action in a fresh prompt: save saves, discard closes without
   saving, and cancel leaves the edited file open. Check focus returns to the
   editor after dismissal.
5. Use the existing prompt navigation keys and Enter to select an action;
   Escape must select cancel. Also exercise a prompt with an explicitly typed
   cancel button whose label is not "Cancel", and one without a cancel action.
6. Repeat with `use_system_prompts` enabled and disabled; both settings must
   use the same themed Android prompt.

## Failure mode if regressed

A lost size or scroll gate brings back clipped or unreachable content. Lost
button indices can make a cancel save or discard instead; dropping the typed
cancel index leaves prompts with custom cancel labels stuck on Escape.
