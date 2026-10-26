//! Bridge between gpui's `CursorStyle` and Android's `PointerIcon`.
//!
//! On platforms with a hardware pointer (BT mouse, attached trackpad) the
//! tablet decides which cursor to draw via `View.setPointerIcon`. The view
//! lives in the JVM so this module hops through JNI to set it on the
//! activity's decor view whenever gpui asks for a cursor change.

use std::sync::atomic::{AtomicI32, Ordering};

use android_activity::AndroidApp;
use anyhow::Context as _;
use gpui::CursorStyle;
use jni::{JavaVM, objects::JObject, sys::jint};

/// Last-pushed Android `PointerIcon.TYPE_*` value. Process-wide
/// atomic so the `nativeOnExtraActivityCreated` handler can read it
/// and push the current style to a freshly-registered
/// `ExtraWindowActivity` (otherwise the new activity's cursor
/// sprite stays at its compile-time default `STYLE_ARROW` until
/// gpui's hover detection triggers the next style change, which
/// may not happen for a while if the cursor sits over a region
/// whose style matches the cached value). `0` = unset (no JNI
/// push has happened yet this process lifetime).
static LAST_STYLE_ICON_TYPE: AtomicI32 = AtomicI32::new(0);

/// Read the last-pushed cursor icon-type id. `nativeOnExtraActivityCreated`
/// uses this to forward the current style to a newly-spawned
/// activity. Returns `None` when no style has been pushed yet
/// (initial app launch with cursor never having moved over
/// anything style-changing).
pub(crate) fn last_pushed_icon_type() -> Option<jint> {
    let value = LAST_STYLE_ICON_TYPE.load(Ordering::Acquire);
    if value == 0 { None } else { Some(value) }
}

/// Map a gpui `CursorStyle` to one of Android's `PointerIcon.TYPE_*`
/// constants. Values come from
/// `frameworks/base/core/java/android/view/PointerIcon.java`.
fn pointer_icon_type(style: CursorStyle) -> jint {
    match style {
        CursorStyle::Arrow => 1000,                        // TYPE_DEFAULT
        CursorStyle::IBeam => 1008,                        // TYPE_TEXT
        CursorStyle::IBeamCursorForVerticalLayout => 1009, // TYPE_VERTICAL_TEXT
        CursorStyle::Crosshair => 1007,                    // TYPE_CROSSHAIR
        CursorStyle::ClosedHand => 1021,                   // TYPE_GRABBING
        CursorStyle::OpenHand => 1020,                     // TYPE_GRAB
        CursorStyle::PointingHand => 1002,                 // TYPE_HAND
        CursorStyle::ResizeLeft
        | CursorStyle::ResizeRight
        | CursorStyle::ResizeLeftRight
        | CursorStyle::ResizeColumn => 1014, // TYPE_HORIZONTAL_DOUBLE_ARROW
        CursorStyle::ResizeUp
        | CursorStyle::ResizeDown
        | CursorStyle::ResizeUpDown
        | CursorStyle::ResizeRow => 1015, // TYPE_VERTICAL_DOUBLE_ARROW
        CursorStyle::ResizeUpRightDownLeft => 1017,        // TYPE_TOP_RIGHT_DIAGONAL_DOUBLE_ARROW
        CursorStyle::ResizeUpLeftDownRight => 1016,        // TYPE_TOP_LEFT_DIAGONAL_DOUBLE_ARROW
        CursorStyle::OperationNotAllowed => 1012,          // TYPE_NO_DROP
        CursorStyle::DragLink => 1010,                     // TYPE_ALIAS
        CursorStyle::DragCopy => 1011,                     // TYPE_COPY
        CursorStyle::ContextualMenu => 1001,               // TYPE_CONTEXT_MENU
    }
}

/// Set the system pointer icon on the activity's decor view via JNI.
/// Call from the main thread; android-activity's `android_main` runs on
/// the Android UI thread so this is safe in our normal event flow.
///
/// gpui's `reset_cursor_style` fires on every input event, so we cache the
/// last style and skip the JNI round-trip when nothing changed.
pub(crate) fn set_pointer_icon(android_app: &AndroidApp, style: CursorStyle) {
    let icon_type = pointer_icon_type(style);
    if LAST_STYLE_ICON_TYPE.load(Ordering::Acquire) == icon_type {
        return;
    }
    if let Err(err) = set_pointer_icon_inner(android_app, style) {
        log::warn!("set_pointer_icon({style:?}) failed: {err:#}");
        return;
    }
    LAST_STYLE_ICON_TYPE.store(icon_type, Ordering::Release);
}

fn set_pointer_icon_inner(android_app: &AndroidApp, style: CursorStyle) -> anyhow::Result<()> {
    let icon_type = pointer_icon_type(style);
    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };

    let window = env
        .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])?
        .l()?;
    let decor = env
        .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?
        .l()?;
    let pointer_icon_class = env.find_class("android/view/PointerIcon")?;
    let pointer_icon = env
        .call_static_method(
            &pointer_icon_class,
            "getSystemIcon",
            "(Landroid/content/Context;I)Landroid/view/PointerIcon;",
            &[(&activity).into(), icon_type.into()],
        )?
        .l()?;
    env.call_method(
        &decor,
        "setPointerIcon",
        "(Landroid/view/PointerIcon;)V",
        &[(&pointer_icon).into()],
    )?;

    // While the activity has pointer capture, the system PointerIcon
    // we just set is hidden. Push the icon-type id to every live
    // Activity so each one's SurfaceControl-based cursor overlay
    // renders the matching sprite. Fanning out (instead of pushing
    // only to MainActivity) means a focused ExtraWindowActivity
    // also gets the IBeam / link / etc. style updates from gpui's
    // `set_cursor_style` — without this, the settings/picker
    // window's sprite stayed stuck at the default arrow because
    // gpui calls set_cursor_style at the Platform layer (no per-
    // window context) and we were only routing to the primary.
    // Each Activity's Kotlin method no-ops when its overlay isn't
    // live, so unfocused windows safely accept the push.
    let _ = env.call_method(
        &activity,
        "setCapturedCursorStyle",
        "(I)V",
        &[icon_type.into()],
    );
    let extra_ids: Vec<u64> = crate::multi_window::extra_activity_ids();
    for id in extra_ids {
        if let Some(activity_ref) = crate::multi_window::extra_activity_for(id) {
            let _ = env.call_method(
                activity_ref.as_obj(),
                "setCapturedCursorStyle",
                "(I)V",
                &[icon_type.into()],
            );
        }
    }
    Ok(())
}

/// Move the SurfaceControl cursor overlay to a touch-trackpad cursor
/// position. Called from the touch state machine while VNC-style
/// trackpad mode is on. `extra_window_id` selects which Activity
/// owns the cursor sprite: `None` → primary (`MainActivity`),
/// `Some(id)` → the registered `ExtraWindowActivity` for that
/// window. Coordinates are physical pixels in the target
/// decorView's space.
pub(crate) fn move_trackpad_cursor(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    x: f32,
    y: f32,
) {
    if let Err(err) = move_trackpad_cursor_inner(android_app, extra_window_id, x, y) {
        log::warn!("move_trackpad_cursor failed: {err:#}");
    }
}

fn move_trackpad_cursor_inner(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    x: f32,
    y: f32,
) -> anyhow::Result<()> {
    let vm =
        unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast()) }.context("JavaVM::from_raw")?;
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread")?;
    match extra_window_id {
        Some(id) => {
            let Some(activity_ref) = crate::multi_window::extra_activity_for(id) else {
                // Extra activity already torn down — just drop the
                // cursor update silently.
                return Ok(());
            };
            env.call_method(
                activity_ref.as_obj(),
                "setTrackpadCursorPosition",
                "(FF)V",
                &[x.into(), y.into()],
            )
            .context("call ExtraWindowActivity.setTrackpadCursorPosition")?;
        }
        None => {
            let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };
            env.call_method(
                &activity,
                "setTrackpadCursorPosition",
                "(FF)V",
                &[x.into(), y.into()],
            )
            .context("call MainActivity.setTrackpadCursorPosition")?;
        }
    }
    Ok(())
}

/// Tell Kotlin to show or hide the SurfaceControl cursor overlay
/// for touch-trackpad mode. The platform reconcile loop broadcasts
/// the same `active` value to every window (primary + each
/// registered extra) when the `TRACKPAD_MODE_ENABLED` atomic
/// flips, so the cursor sprite appears on whichever window the
/// user is currently interacting with. `extra_window_id` routes
/// to the right Activity; `None` → primary, `Some(id)` → extra.
pub(crate) fn set_trackpad_mode_active(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    active: bool,
) {
    if let Err(err) = set_trackpad_mode_active_inner(android_app, extra_window_id, active) {
        log::warn!("set_trackpad_mode_active({active}) failed: {err:#}");
    }
}

fn set_trackpad_mode_active_inner(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    active: bool,
) -> anyhow::Result<()> {
    let vm =
        unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast()) }.context("JavaVM::from_raw")?;
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread")?;
    match extra_window_id {
        Some(id) => {
            let Some(activity_ref) = crate::multi_window::extra_activity_for(id) else {
                return Ok(());
            };
            env.call_method(
                activity_ref.as_obj(),
                "setTrackpadModeActive",
                "(Z)V",
                &[active.into()],
            )
            .context("call ExtraWindowActivity.setTrackpadModeActive")?;
        }
        None => {
            let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };
            env.call_method(&activity, "setTrackpadModeActive", "(Z)V", &[active.into()])
                .context("call MainActivity.setTrackpadModeActive")?;
        }
    }
    Ok(())
}
