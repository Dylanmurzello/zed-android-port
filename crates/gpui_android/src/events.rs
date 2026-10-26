//! Android input → gpui `PlatformInput` translation.
//!
//! Two entry points:
//! - [`translate_motion_event`] for the primary surface, which receives
//!   NDK `MotionEvent`s through android-activity's native input queue.
//! - [`translate_extra_motion_event`] for extra-window surfaces, which
//!   receive raw `MotionEvent` fields marshaled across the JNI boundary
//!   by `ExtraWindowActivity.forwardTouchEvent`.
//!
//! Source-based routing: Finger input (multi-touch, tap, drag) goes to
//! [`crate::touch`]'s first-class SM. Mouse / stylus / captured-trackpad
//! continue through these dispatchers. Per-modality submodules:
//! - [`mouse`] for mouse buttons + wheel + drag (button mapping + scroll
//!   synthesis).
//! - [`click_track`] for shared click-count + non-primary held-button
//!   state (mouse + touch both use the run-tracking).
//! - [`keyboard`] for hardware keys (entry points
//!   [`translate_key_event`] and [`translate_extra_key_event`]).
//!
//! IME / soft-keyboard composition will land in its own module
//! (see `docs/workarounds/deferred-soft-keyboard.md`).

pub(crate) mod click_track;
pub(crate) mod keyboard;
pub(crate) mod mouse;
pub(crate) mod source;

use android_activity::input::{Axis, MetaState, MotionAction, MotionEvent};
use gpui::{MouseButton, MouseMoveEvent, PlatformInput, point, px};

pub(crate) use keyboard::{translate_extra_key_event, translate_key_event};

use crate::window::AndroidWindowState;

/// Output of `translate_motion_event`. Touch interactions can need to
/// emit more than one synthetic input (the two-finger right-click path
/// emits Up(Left) + Down(Right) + Up(Right)) so the caller drains a
/// small vec rather than a single optional event.
pub(crate) type MotionInputs = Vec<PlatformInput>;

/// Java `MotionEvent.getActionMasked()` constants. We can't reuse
/// `android_activity::input::MotionAction` for the extra-window path
/// because that enum's constructor is private: `MotionEvent`s authored
/// from arbitrary JNI data only carry the integer.
pub(crate) const JAVA_ACTION_DOWN: i32 = 0;
pub(crate) const JAVA_ACTION_UP: i32 = 1;
pub(crate) const JAVA_ACTION_MOVE: i32 = 2;
pub(crate) const JAVA_ACTION_CANCEL: i32 = 3;
pub(crate) const JAVA_ACTION_POINTER_DOWN: i32 = 5;
pub(crate) const JAVA_ACTION_POINTER_UP: i32 = 6;
pub(crate) const JAVA_ACTION_HOVER_MOVE: i32 = 7;
pub(crate) const JAVA_ACTION_SCROLL: i32 = 8;
pub(crate) const JAVA_ACTION_HOVER_ENTER: i32 = 9;

/// Convert an Android `MotionEvent` (mouse / stylus / finger) into one
/// or more gpui `PlatformInput`s. Coordinates arrive from Android in
/// physical pixels; gpui expects logical, so we divide by the active
/// `scale_factor` here.
///
/// Finger input is delegated to [`crate::touch`]'s SM and never reaches
/// the match arms below. The arms only fire for Mouse / Stylus /
/// Touchpad sources, which on Android always present as single-pointer
/// events (no multi-touch ACTION_POINTER_*) — that's why the
/// multi-pointer arms are absent from this function.
pub(crate) fn translate_motion_event(
    event: &MotionEvent,
    scale_factor: f32,
    state: &mut AndroidWindowState,
) -> MotionInputs {
    if event.pointer_count() == 0 {
        return MotionInputs::new();
    }
    let primary = event.pointer_at_index(0);
    let position = point(
        px(primary.x() / scale_factor),
        px(primary.y() / scale_factor),
    );
    let modifiers = keyboard::modifiers_from_meta(event.meta_state());
    let button_state = event.button_state().0 as i32;
    let pressed_mouse_button = mouse::button_from_state(button_state);
    let input_source = source::classify(event);

    // Track the most recent input modality so UI components can widen
    // hit zones for touch without affecting mouse precision. In
    // trackpad mode, finger input is synthesized into mouse events
    // and the user controls a precise virtual cursor — touch-friendly
    // widening would actively hurt (splitters / scrollbars capture
    // clicks meant for adjacent buttons like tab-close). Treat
    // finger-driven trackpad mode as mouse for hit-test purposes.
    let is_touch_input =
        input_source == source::InputSource::Finger && !crate::ime::trackpad_mode_enabled();
    state
        .last_input_was_touch
        .store(is_touch_input, std::sync::atomic::Ordering::Relaxed);

    // Finger input routes through the first-class touch state machine.
    // Mouse / stylus / captured-trackpad continue through the match
    // arms below.
    if input_source == source::InputSource::Finger {
        return crate::touch::dispatch_primary(state, event, scale_factor);
    }

    let mut out = MotionInputs::new();
    match event.action() {
        MotionAction::Down => {
            // If Android resolved a non-primary mouse / trackpad button
            // (right, middle, side-back, side-forward, or trackpad two-
            // finger tap surfacing as BUTTON_SECONDARY), emit the
            // corresponding Down and latch it so the matching Up
            // resolves to the same button.
            if let Some(button) = pressed_mouse_button
                && button != MouseButton::Left
            {
                let click_count = state
                    .clicks
                    .next_click_count(button, position, input_source);
                state.clicks.mark_non_primary_down(button);
                out.push(mouse::button_down(button, position, modifiers, click_count));
                return out;
            }
            // Mouse left button down. Latch position + time and emit
            // Down(Left) immediately for instant click feedback.
            let click_count =
                state
                    .clicks
                    .next_click_count(MouseButton::Left, position, input_source);
            state.clicks.record_primary_down(position);
            out.push(mouse::button_down(
                MouseButton::Left,
                position,
                modifiers,
                click_count,
            ));
        }
        MotionAction::Up => {
            // Mouse button release. Resolve the latched click.
            if let click_track::UpOutcome::Emit(button) = state.clicks.finalize_up() {
                out.push(mouse::button_up(button, position, modifiers));
            }
        }
        MotionAction::Move => {
            // Mouse drag with a button held. Report the held button as
            // pressed_button. Falls back to Left for plain mouse Left-
            // drag (button_state still reports Left during the drag).
            let pressed = state
                .clicks
                .current_non_primary()
                .or(pressed_mouse_button)
                .unwrap_or(MouseButton::Left);
            out.push(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: Some(pressed),
                modifiers,
            }));
        }
        MotionAction::HoverEnter | MotionAction::HoverMove => {
            // Mouse entered or moved over the window without a button
            // held. Both surface as `MouseMove { pressed_button: None }`
            // so gpui's hover-tracking state stays current. `HoverExit`
            // falls through silently: native backends generally stop
            // reporting hover when the cursor leaves the window rather
            // than synthesizing a "moved-to-infinity" event.
            out.push(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            }));
        }
        MotionAction::Cancel => {
            // End the gesture cleanly. Emit the up for whichever button
            // was held (or Left as a default) with click_count=0 so
            // listeners can distinguish a real click.
            let held = state
                .clicks
                .current_non_primary()
                .unwrap_or(MouseButton::Left);
            state.clicks.reset_all();
            out.push(PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: held,
                position,
                modifiers,
                click_count: 0,
            }));
        }
        MotionAction::Scroll => {
            let vscroll = primary.axis_value(Axis::Vscroll);
            let hscroll = primary.axis_value(Axis::Hscroll);
            if let Some(event) = mouse::wheel_scroll(vscroll, hscroll, position, modifiers) {
                out.push(event);
            }
        }
        _ => {}
    }
    out
}

/// Touch translator for events arriving on extra `SurfaceView`s (i.e.
/// secondary gpui windows hosted by `multi_window`). The primary path
/// uses [`translate_motion_event`] which consumes android-activity's
/// NDK-backed `MotionEvent`; this one takes the raw fields we marshal
/// across the JNI boundary in `ExtraWindowActivity.forwardTouchEvent`.
///
/// Source classification isn't yet plumbed across the JNI bridge, so we
/// route touch-like actions (Down/Up/Move/Cancel/PointerDown/PointerUp)
/// through the touch SM unconditionally and reserve the mouse-only
/// actions (HOVER_*, SCROLL) for the mouse path below.
#[allow(clippy::too_many_arguments)]
pub(crate) fn translate_extra_motion_event(
    window_id: u64,
    action_masked: i32,
    action_index: i32,
    meta_state: i32,
    button_state: i32,
    vscroll: f32,
    hscroll: f32,
    positions: &[(f32, f32, i32)],
    scale_factor: f32,
    state: &mut AndroidWindowState,
) -> MotionInputs {
    if positions.is_empty() {
        return Vec::new();
    }

    // Touch-actions go through the touch SM (per-window keyed). Mouse-
    // only actions (HOVER_ENTER/MOVE, SCROLL) fall through to the legacy
    // mouse path so a USB mouse plugged in while Settings is open still
    // hovers + scrolls correctly.
    let is_touch_action = matches!(
        action_masked,
        JAVA_ACTION_DOWN
            | JAVA_ACTION_UP
            | JAVA_ACTION_MOVE
            | JAVA_ACTION_CANCEL
            | JAVA_ACTION_POINTER_DOWN
            | JAVA_ACTION_POINTER_UP
    );
    // Same trackpad-mode override as the primary surface path:
    // virtual trackpad treats finger as mouse, no touch-friendly
    // hit-zone widening.
    state.last_input_was_touch.store(
        is_touch_action && !crate::ime::trackpad_mode_enabled(),
        std::sync::atomic::Ordering::Relaxed,
    );
    if is_touch_action {
        return crate::touch::dispatch_extra(
            state,
            window_id,
            action_masked,
            action_index,
            meta_state,
            button_state,
            vscroll,
            hscroll,
            positions,
            scale_factor,
        );
    }

    let (raw_x, raw_y, _id) = positions[0];
    let position = point(px(raw_x / scale_factor), px(raw_y / scale_factor));
    let modifiers = keyboard::modifiers_from_meta(MetaState(meta_state as u32));

    let mut out = Vec::new();
    match action_masked {
        JAVA_ACTION_HOVER_ENTER | JAVA_ACTION_HOVER_MOVE => {
            out.push(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            }));
        }
        JAVA_ACTION_SCROLL => {
            if let Some(event) = mouse::wheel_scroll(vscroll, hscroll, position, modifiers) {
                out.push(event);
            }
        }
        _ => {}
    }
    out
}
