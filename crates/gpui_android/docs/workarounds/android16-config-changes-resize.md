# Android 16 freeform resize fires Activity recreation by default

**Status:** Active
**Phase / Commit:** L7
**Files:** `crates/gpui_android/examples/zed_android/android/app/src/main/AndroidManifest.xml`

## Problem

In freeform / desktop windowing mode, every drag of the OS resize handle
fires a configuration change. By default, Android **destroys and recreates
the Activity** on every config change. Default behavior would be:

1. User drags the resize handle on the Settings window
2. Activity destroyed → `onDestroy` → JNI `nativeOnExtraActivityDestroyed`
   → `OsClosed` event posted to gpui
3. gpui drops the Window — the user's Settings UI vanishes mid-resize
4. New Activity instance launches with same Intent → tries to re-attach to
   a Window gpui has already torn down
5. Window stays gone. User just lost their settings dialog by *resizing it*.

End-state: every resize closes the secondary window. Unusable.

## Constraint

From [Android 16 freeform docs](https://developer.android.com/develop/ui/large-screens/multi-window-support#desktop-experiences):

> Orientation changes and resizing will result in Activity recreation by
> default. To ensure a good user experience, it is critical that app state
> is preserved through these configuration changes.

Two approaches: (1) implement `onSaveInstanceState` for full state restore,
or (2) declare every config we want to handle ourselves so the OS delivers
`onConfigurationChanged()` instead of recreating. Option 1 doesn't play
with our Vulkan-backed Rust runtime — gpui state isn't `Bundle`-serializable.
Option 2 is universal and cheap.

## Solution

Exhaustive `configChanges` declaration on both Activities:

```xml
android:configChanges="orientation|screenSize|smallestScreenSize|screenLayout
                      |density|keyboard|keyboardHidden|navigation|uiMode
                      |colorMode|fontScale|fontWeightAdjustment|layoutDirection
                      |locale|mcc|mnc|touchscreen"
```

Each entry:

| Code | Why declared |
|---|---|
| `orientation` | Rotation |
| `screenSize` / `smallestScreenSize` / `screenLayout` | Resize, drag-resize |
| `density` | Cross-display moves (HDR tablet ↔ SDR external monitor) |
| `colorMode` | Same — wide-gamut display vs SDR |
| `keyboard` / `keyboardHidden` | Hardware keyboard attach/detach |
| `navigation` | Stylus / d-pad presence |
| `uiMode` | Day↔night, normal↔car↔TV |
| `fontScale` / `fontWeightAdjustment` | Accessibility font settings |
| `layoutDirection` | RTL toggle |
| `locale` | Language change |
| `mcc` / `mnc` | SIM swap, roaming |
| `touchscreen` | Touch input availability |

Verbose but cheap. With this declared, the OS delivers
`onConfigurationChanged()` instead of recreating, the surface fires
`surfaceChanged` (not `surfaceDestroyed/Created`), the existing resize
event path handles it, the `GlobalRef` stays valid, gpui Window survives.

## Why this works

Each `configChanges` token tells the framework "I'll handle this myself,
deliver a callback instead of recreating." We don't actually have to do
anything in `onConfigurationChanged` — the SurfaceView's
`SurfaceHolder.Callback.surfaceChanged` already fires with the new size,
which triggers our existing resize path.

## Failure mode if regressed

- Drag-resize the Settings window in DeX/desktop mode → window content
  immediately disappears. Closes silently mid-resize.
- Could also trigger from device rotation if `orientation` is missing.
- `density` missing: moving the window from internal display to external
  monitor closes it.

Test: aggressive drag-resize after every manifest change. If the window
survives, configChanges is right.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
