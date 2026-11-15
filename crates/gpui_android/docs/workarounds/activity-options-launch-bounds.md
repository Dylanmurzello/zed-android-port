# `ActivityOptions.setLaunchBounds` for initial freeform window rect

**Status:** Active
**Phase / Commit:** L7e
**Files:**
- `crates/gpui_android/src/multi_window.rs` (`LaunchSize`) and `MainActivity.launchExtraWindow`
- `crates/gpui_android/src/platform.rs` (`compute_launch_size`)

## Problem

By default, when you `startActivity` a new task in freeform / desktop
windowing mode, the OS picks the initial window size and position. On
Samsung tablets in DeX mode that's a 1601×1313 rect at a hardcoded offset.
On Pixel desktop windowing it's something else. We can't predict where
"Open Settings" will spawn or how big it'll be.

gpui callers DO want to express intent: `cx.open_window(WindowParams {
bounds: ..., ... })` already carries a requested size and origin. We
were ignoring it on Android.

## Constraint

The Activity tag in the manifest can't carry a default size — manifest is
static. Bounds must come at startActivity time.

`Intent` itself doesn't carry bounds either. The mechanism is
`ActivityOptions.makeBasic().setLaunchBounds(Rect)` — pass the resulting
`Bundle` as a second argument to `startActivity(Intent, Bundle)`.

API floor: `ActivityOptions.setLaunchBounds(Rect)` is API 24+. We're at
the app's `minSdk`, fine.

## Solution

`compute_launch_size` turns the size of gpui's `WindowParams.bounds`
(logical pixels at the primary's scale factor) into device pixels, and
`launch_extra_activity` passes it to `MainActivity.launchExtraWindow` over
JNI. That method places the window: it takes the display's maximum
`WindowMetrics`, removes the insets of `systemBars() | displayCutout()`
ignoring their visibility, shrinks the size to fit that area and centers it
there, then starts the Activity with
`ActivityOptions.makeBasic().setLaunchBounds(rect).toBundle()`.

A size that isn't positive starts the Activity without options, and the OS
picks the rectangle.

The fit matters because Android doesn't shrink a launch rectangle that
doesn't fit the area clear of the system bars: it moves the rectangle to
that area's top. Ctrl+Shift+N asks for a window at least as large as the screen, and
until 0.17.2 the app centered it on the whole screen, so One UI outside DeX
put it at `[0,45][2960,1893]` on the reference tablet: 45 px down, below
the status bar, with its bottom edge and Zed's status bar off the screen
(2026-10-31).

## Why this works

`ActivityOptions.makeBasic().setLaunchBounds(Rect).toBundle()` is the
documented API for "tell the system where to put this freeform window
when it launches." The OS honors it on freeform-capable devices and
ignores it on phone fullscreen.

`gpui::WindowParams.bounds.origin` is currently ignored — gpui uses it as
"where to place the window in screen coords" but that's macOS-flavored
semantics. On Android we always center; explicit positioning would need
a separate API surface (deferred).

## Failure mode if regressed

- **Without launch bounds:** Settings always opens at OS-default size and
  position. Visually inconsistent across devices.
- **Wrong scale factor:** size off by 2x or 0.5x — Settings opens too big
  or too small.
- **Wrong screen coords:** window opens off-screen or at the corner. On
  some devices this triggers a "snapped to edge" auto-resize.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
- L7e (deferred): real `set_title()` is wired (this file's sibling
  `set_extra_activity_title`); `is_maximized()`, `zoom()`, `minimize()`,
  `toggle_fullscreen()` remain stubs — OS chrome handles them via direct
  user interaction with the chrome buttons.
