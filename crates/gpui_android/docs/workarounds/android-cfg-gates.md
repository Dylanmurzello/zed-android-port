# cfg(target_os = "android") gates in upstream files

**Status:** Active

**Files:** `crates/workspace/src/pane_group.rs`, `crates/workspace/src/dock.rs`, `crates/ui/src/components/scrollbar.rs`, `crates/editor/src/element.rs`, `crates/workspace/src/workspace.rs`, `crates/settings_ui/src/page_data.rs`, `crates/onboarding/src/basics_page.rs`

## Problem

A few upstream files need behavior only the Android build wants: wider targets for a finger, a drag the splitter or a dock handle claims before gpui's touch gestures scroll the content, a quit that keeps its windows, and Android's own settings and onboarding sections.

## Constraint

`Window::last_input_was_touch` is not an Android signal: gpui_ios and gpui_web emit `PlatformInput::Touch` too, so a hit zone keyed on it alone changes those builds. And the first Android window is the Activity's surface, which the process cannot open again.

## Solution

Every behavior change is behind `cfg(target_os = "android")`:

- Touch hit zones, gated as `cfg!(target_os = "android") && window.last_input_was_touch()`: the pane splitter's 16 px touch hitbox (a hitbox of its own with `HitboxBehavior::Normal`, so a menu or a modal over the divider still wins the hit test and the content beside it keeps its presses), the 20 px dock handle, a scrollbar track press that grabs the thumb, the editor's scrollbar found by bounds.
- `TouchDragEvent` listeners that claim a finger drag for the splitter, the dock handle and the scrollbar: `#[cfg(target_os = "android")]` blocks.
- `prepare_windows_to_quit` keeps the prepared windows when a later window cancels the quit: `#[cfg(not(target_os = "android"))]` on the removal loop.
- The Android settings page (`page_data/android.rs`) and the onboarding sections (`AndroidBasicsSections`) are added behind the cfg; the Privacy toggles stay on every platform, with telemetry off by default in the app's default settings.

Additions other platforms never call stay ungated (`Panel::snap_size`, `App::active_drag_value`, `CosmicTextSystem::load_fonts_dir`, `Workspace::drag_dock_edge`). The audio, livekit and call gates this document once described are gone: the app does not build those crates.

## Failure mode if regressed

A touch zone without the cfg changes gpui_ios and gpui_web. A Zed merge that drops a gate shows up as a desktop behavior change, or on Android as a splitter that starts a drag under an open menu, or a cancelled quit that leaves an empty Activity.

## See also

- [ahead-of-upstream.md](../ahead-of-upstream.md) for the touch gestures these zones build on.
