# No default focus highlight on extra-window surfaces

**Status:** Active
**Files:** `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/ExtraWindowActivity.kt`

## Problem

Settings and every other `ExtraWindowActivity` window looked washed out next
to the main window whenever a keyboard, mouse or trackpad was in use. Each
color was the theme's color mixed with about 16% white: the editor
background (40,44,53) showed as (75,78,85), the title bar (28,28,40) as
(64,64,74). Tapping the window with a finger cleared it until the next key
press or mouse click.

## Constraint

The window's `SurfaceView` has to be focusable: key events and the IME
target it. Since Android 8, while the window is out of touch mode Android
draws a translucent "default focus highlight" over the focused view when
the view has no focused-state drawable of its own. The surface fills the
window, so the highlight covers all of it. It's drawn in the window layer
above the `SurfaceView`, so the surface's `PixelFormat.OPAQUE` (the earlier
fix for this tint, [6adf7b808e]) doesn't affect it.

`MainActivity` doesn't show it: GameActivity's surface isn't focusable,
since the keyboard goes through the 1×1 `ImeHostView` instead.

## Solution

```kotlin
isFocusable = true
isFocusableInTouchMode = true
defaultFocusHighlightEnabled = false
```

`setDefaultFocusHighlightEnabled` is API 26, the app's `minSdk`.

## Why this works

Focus and key dispatch don't depend on the highlight; disabling it only
stops Android from drawing it.

## Failure mode if regressed

Extra windows get a whitish veil after keyboard, mouse or trackpad input,
and it disappears on a finger tap.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
- [Extra-window input parity](extra-window-input-parity.md)

[6adf7b808e]: https://github.com/Dylanmurzello/zed-android-port/commit/6adf7b808e
