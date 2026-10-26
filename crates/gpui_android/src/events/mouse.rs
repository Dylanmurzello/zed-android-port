//! Mouse input translation. Buttons, wheel, hover, drag.
//!
//! Android pre-resolves trackpad / mouse multi-finger gestures and
//! physical buttons into `MotionEvent.button_state`. A two-finger tap on
//! Samsung's keyboard-cover trackpad arrives here as a single pointer with
//! `BUTTON_SECONDARY` set; physical right-click on a USB mouse arrives the
//! same way. We honor `button_state` directly so the user doesn't have
//! to do anything special with a trackpad.

use gpui::{
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, NavigationDirection, PlatformInput,
    Pixels, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px,
};

/// Android `MotionEvent.BUTTON_*` bit constants. NDK definitions live in
/// `ndk_sys::AMOTION_EVENT_BUTTON_*`; these mirrors are kept in sync so
/// the extra-window JNI path (which receives `button_state` as a raw
/// `i32`) can use the same bit checks as the primary path.
pub(crate) const ANDROID_BUTTON_PRIMARY: i32 = 1 << 0;
/// `BUTTON_SECONDARY`: right mouse button, or trackpad two-finger tap.
pub(crate) const ANDROID_BUTTON_SECONDARY: i32 = 1 << 1;
/// `BUTTON_TERTIARY`: middle mouse button (typically wheel click).
pub(crate) const ANDROID_BUTTON_TERTIARY: i32 = 1 << 2;
/// `BUTTON_BACK`: mouse side button pointing toward the user. Maps to
/// gpui's `MouseButton::Navigate(NavigationDirection::Back)`.
pub(crate) const ANDROID_BUTTON_BACK: i32 = 1 << 3;
/// `BUTTON_FORWARD`: mouse side button pointing away from the user.
pub(crate) const ANDROID_BUTTON_FORWARD: i32 = 1 << 4;

/// Map an Android `button_state` bitfield to the gpui `MouseButton`
/// that should be reported. Priority: primary > tertiary > secondary >
/// back > forward. Returns `None` when no flag is set (touch gesture
/// with no physical button concept).
pub(crate) fn button_from_state(state: i32) -> Option<MouseButton> {
    if state & ANDROID_BUTTON_PRIMARY != 0 {
        Some(MouseButton::Left)
    } else if state & ANDROID_BUTTON_TERTIARY != 0 {
        Some(MouseButton::Middle)
    } else if state & ANDROID_BUTTON_SECONDARY != 0 {
        Some(MouseButton::Right)
    } else if state & ANDROID_BUTTON_BACK != 0 {
        Some(MouseButton::Navigate(NavigationDirection::Back))
    } else if state & ANDROID_BUTTON_FORWARD != 0 {
        Some(MouseButton::Navigate(NavigationDirection::Forward))
    } else {
        None
    }
}

/// Build a `MouseDown` for any button.
///
/// `first_mouse: false` because there's no window-focus concept on
/// Android; setting `true` would make every click look like a focus-
/// the-window-first click, which `ClickEvent::first_focus` returns as
/// true. Listeners like ProjectPanel's on_click bail on a "first focus"
/// click, so files would never open / folders would never expand.
///
/// For non-primary (right / middle / nav) buttons, the caller also flags
/// the touch state machine via
/// [`crate::events::touch::mark_non_primary_down`] so the matching Up
/// resolves to the right button instead of the default Left.
pub(crate) fn button_down(
    button: MouseButton,
    position: Point<Pixels>,
    modifiers: Modifiers,
    click_count: usize,
) -> PlatformInput {
    PlatformInput::MouseDown(MouseDownEvent {
        button,
        position,
        modifiers,
        click_count,
        first_mouse: false,
    })
}

/// Build the `MouseUp` paired with a previously-emitted Down. Caller
/// decides the button (typically driven by
/// [`crate::events::touch::finalize_up`]).
pub(crate) fn button_up(
    button: MouseButton,
    position: Point<Pixels>,
    modifiers: Modifiers,
) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button,
        position,
        modifiers,
        click_count: 1,
    })
}

/// Build the `ScrollWheelEvent` for an `ACTION_SCROLL` event (mouse
/// wheel, mouse-equivalent trackpad scroll).
///
/// Android's `AXIS_VSCROLL` is +1.0 for "up / forward" (wheel rotated
/// away from the user); gpui's `ScrollDelta::Lines.y` is also +1.0 for
/// "scroll content up" (matches `terminal::mappings::mouse::is_scroll_up`
/// and macOS's raw `scrollingDeltaY`). The two conventions align: pass
/// through unchanged. A prior `-vscroll` flip caused BT-mouse scrolls
/// to feel backwards, reported via the @-mention loop on 2026-05-13.
pub(crate) fn wheel_scroll(
    vscroll: f32,
    hscroll: f32,
    position: Point<Pixels>,
    modifiers: Modifiers,
) -> Option<PlatformInput> {
    if vscroll == 0.0 && hscroll == 0.0 {
        return None;
    }
    Some(PlatformInput::ScrollWheel(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(hscroll, vscroll)),
        modifiers,
        touch_phase: TouchPhase::Moved,
    }))
}

/// Pixel-precision scroll synthesized from multi-touch centroid delta.
/// Trackpad two-finger swipes that arrive as multi-pointer ACTION_MOVE
/// (as from Samsung's cover trackpad) feed into this. Caller does the scale-
/// factor divide and supplies logical-pixel deltas.
pub(crate) fn pixel_scroll(
    delta: Point<Pixels>,
    position: Point<Pixels>,
    modifiers: Modifiers,
) -> PlatformInput {
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Pixels(delta),
        modifiers,
        touch_phase: TouchPhase::Moved,
    })
}
