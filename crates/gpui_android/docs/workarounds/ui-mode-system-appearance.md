# Report Android UI mode as window appearance

**Status:** Active
**Phase / Commit:** `caa82bae9a` — Report Android UI mode as window appearance
**Files:** `crates/gpui_android/src/window.rs`, `crates/gpui_android/src/platform.rs`

## Problem

Theme system's "system" mode always picked One Light, regardless of the
device's dark/light setting. Toggling system theme in Android settings did
nothing to Zed.

## Constraint

GPUI's appearance system (`WindowAppearance::Light`/`::Dark`) drives the
theme registry's "system" branch. We were hardcoding
`WindowAppearance::Light` because there was no Android-side bridge for the
UI mode. We also need to react to mid-session changes — user flips dark
mode in quick settings, Zed's theme should follow.

## Solution

Two reads:

1. **At startup**, read `android_app.config().ui_mode_night()` and seed
   the initial appearance value from it.
2. **On `MainEvent::ConfigChanged`**, re-read and emit a new appearance
   if it changed. gpui's `appearance_changed` callback fires on the next
   frame, theme registry picks up the new value.

## Why this works

- `ui_mode_night()` returns the same UI_MODE_NIGHT_YES/NO/UNDEFINED enum
  Android resolves from the system setting + per-app overrides. Single
  source of truth.
- ConfigChanged is the canonical event for "the device's display config
  changed in some way" — covers dark mode flips, language changes,
  font scale changes, etc.

## Failure mode if regressed

- Hardcode `Light` → users in dark mode get blinded by white themes.
- Skip ConfigChanged handler → first launch is correct but mid-session
  flips don't propagate.

## See also

- [android16-config-changes-resize.md](android16-config-changes-resize.md)
