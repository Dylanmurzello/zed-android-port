//! Bridge between gpui's `CursorStyle` and Android's `PointerIcon`. Each
//! window's `ZdroidWindow.setCursorStyle` sets it on its decor view.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use android_activity::AndroidApp;
use gpui::CursorStyle;
use jni::sys::jint;
use util::ResultExt as _;

/// `PointerIcon.TYPE_NULL`, which has no image.
const TYPE_NULL: jint = 0;
/// `PointerIcon.TYPE_DEFAULT`.
const TYPE_DEFAULT: jint = 1000;

/// The `PointerIcon.TYPE_*` of gpui's last style, 0 for none: an unchanged style
/// skips JNI. gpui resets the cursor style on every input event.
static LAST_STYLE_ICON_TYPE: AtomicI32 = AtomicI32::new(0);

/// Whether the cursor is hidden until the pointer moves. Android's own fade on a
/// key press needs an input connection, which `ImeHostView` refuses while a
/// hardware keyboard is connected, and it wouldn't follow Zed's `hide_mouse`.
static HIDDEN: AtomicBool = AtomicBool::new(false);

/// What a window opened now should show.
pub(crate) fn last_pushed_icon_type() -> Option<jint> {
    if HIDDEN.load(Ordering::Acquire) {
        return Some(TYPE_NULL);
    }
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

pub(crate) fn set_pointer_icon(android_app: &AndroidApp, style: CursorStyle) {
    let icon_type = pointer_icon_type(style);
    let changed = LAST_STYLE_ICON_TYPE.swap(icon_type, Ordering::AcqRel) != icon_type;
    // A hidden cursor stays hidden, and shows this style when the pointer moves.
    if changed && !HIDDEN.load(Ordering::Acquire) {
        push_icon_type(android_app, icon_type);
    }
}

pub(crate) fn hide_until_pointer_moves(android_app: &AndroidApp) {
    if !HIDDEN.swap(true, Ordering::AcqRel) {
        push_icon_type(android_app, TYPE_NULL);
    }
}

pub(crate) fn show_after_pointer_moved() {
    if !HIDDEN.swap(false, Ordering::AcqRel) {
        return;
    }
    let Some(android_app) = crate::jni_env::android_app().log_err() else {
        return;
    };
    let icon_type = last_pushed_icon_type().unwrap_or(TYPE_DEFAULT);
    push_icon_type(android_app, icon_type);
}

pub(crate) fn is_visible() -> bool {
    !HIDDEN.load(Ordering::Acquire)
}

fn push_icon_type(android_app: &AndroidApp, icon_type: jint) {
    crate::jni_env::call_all_windows(android_app, "setCursorStyle", "(I)V", &[icon_type.into()]);
}
