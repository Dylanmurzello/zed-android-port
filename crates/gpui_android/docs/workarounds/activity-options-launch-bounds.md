# `ActivityOptions.setLaunchBounds` for initial freeform window rect

**Status:** Active
**Phase / Commit:** L7e
**Files:**
- `crates/gpui_android/src/multi_window.rs` (`LaunchBounds`) and `MainActivity.launchExtraWindow`
- `crates/gpui_android/src/platform.rs` (`compute_launch_bounds`)

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

Translate gpui's `WindowParams.bounds` (logical pixels at the primary's
scale factor) into device-pixel screen coordinates centered on the primary
display:

```rust
fn compute_launch_bounds(
    &self,
    bounds: &Bounds<Pixels>,
    scale_factor: f32,
) -> Option<crate::multi_window::LaunchBounds> {
    let width_px = (bounds.size.width.as_f32() * scale_factor).round() as i32;
    let height_px = (bounds.size.height.as_f32() * scale_factor).round() as i32;
    if width_px <= 0 || height_px <= 0 { return None; }
    let nw = self.android_app.native_window()?;
    let screen_w = nw.width() as i32;
    let screen_h = nw.height() as i32;
    let left = ((screen_w - width_px) / 2).max(0);
    let top  = ((screen_h - height_px) / 2).max(0);
    Some(LaunchBounds {
        left, top,
        right: left + width_px,
        bottom: top + height_px,
    })
}
```

JNI side: when bounds are present, route through ActivityOptions.toBundle:

```rust
let rect_class = env.find_class("android/graphics/Rect")?;
let rect_obj = env.new_object(
    &rect_class, "(IIII)V",
    &[JValue::Int(rect.left), JValue::Int(rect.top),
      JValue::Int(rect.right), JValue::Int(rect.bottom)],
)?;
let activity_options_class = env.find_class("android/app/ActivityOptions")?;
let opts = env.call_static_method(
    &activity_options_class, "makeBasic",
    "()Landroid/app/ActivityOptions;", &[],
)?.l()?;
env.call_method(
    &opts, "setLaunchBounds",
    "(Landroid/graphics/Rect;)Landroid/app/ActivityOptions;",
    &[JValue::Object(&rect_obj)],
)?;
let bundle = env.call_method(&opts, "toBundle", "()Landroid/os/Bundle;", &[])?.l()?;
env.call_method(
    &main_activity, "startActivity",
    "(Landroid/content/Intent;Landroid/os/Bundle;)V",
    &[JValue::Object(&intent), JValue::Object(&bundle)],
)?;
```

`None` bounds (degenerate width/height, or no native_window available)
falls back to the unbounded `startActivity(Intent)` overload.

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
