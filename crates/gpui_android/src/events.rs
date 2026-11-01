//! Android input → gpui `PlatformInput` translation.
//!
//! Android's input stack does the gesture work, so this only passes its events
//! on. A finger on the touchscreen goes to gpui's touch recognizer through
//! [`crate::touch`]. A mouse, a stylus and a trackpad, which Android turns into a
//! pointer with button presses and classified gestures, go through [`mouse`].
//! Hardware keys are in [`keyboard`]. See `docs/workarounds/touchpad-input.md`.

pub(crate) mod keyboard;
pub(crate) mod mouse;

use std::time::Instant;

use android_activity::input::{Axis, MetaState, MotionAction, MotionEvent, Source, ToolType};
use gpui::{Pixels, PlatformInput, Point, point, px};

pub(crate) use keyboard::{translate_extra_key_event, translate_key_event};

use crate::window::AndroidWindowState;

/// `MotionEvent.FLAG_CANCELED`: the pointer going up was cancelled, such as a
/// palm the system rejected, so it must not act as a release.
pub(crate) const FLAG_CANCELED: u32 = 0x20;

/// `MotionEvent.CLASSIFICATION_NONE`.
pub(crate) const CLASSIFICATION_NONE: i32 = 0;

/// `MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE`.
pub(crate) const CLASSIFICATION_TWO_FINGER_SWIPE: i32 = 3;

/// One Android `MotionEvent`, from android-activity's input queue in the main
/// window and over JNI otherwise, whose Java object can't become an
/// `android_activity::input::MotionEvent`.
pub(crate) struct Motion {
    pub action: MotionAction,
    /// In `pointers`, the pointer that `PointerDown` or `PointerUp` is about.
    pub action_index: usize,
    pub source: Source,
    pub meta_state: MetaState,
    /// `MotionEvent.getButtonState()`.
    pub button_state: u32,
    /// `MotionEvent.FLAG_CANCELED`.
    pub canceled: bool,
    /// `MotionEvent.getClassification()`. GameActivity's events don't carry it, so
    /// `MainActivity` sends a trackpad's gestures over JNI.
    pub classification: i32,
    /// `AXIS_VSCROLL` and `AXIS_HSCROLL`, for `ACTION_SCROLL`.
    pub vscroll: f32,
    pub hscroll: f32,
    pub pointers: Vec<MotionPointer>,
}

pub(crate) struct MotionPointer {
    pub id: i32,
    pub position: Point<Pixels>,
    pub tool: ToolType,
}

impl Motion {
    pub(crate) fn from_event(event: &MotionEvent) -> Self {
        let pointers = (0..event.pointer_count())
            .map(|index| {
                let pointer = event.pointer_at_index(index);
                MotionPointer {
                    id: pointer.pointer_id(),
                    position: point(px(pointer.x()), px(pointer.y())),
                    tool: pointer.tool_type(),
                }
            })
            .collect();
        let (vscroll, hscroll) = if event.pointer_count() > 0 {
            let pointer = event.pointer_at_index(0);
            (
                pointer.axis_value(Axis::Vscroll),
                pointer.axis_value(Axis::Hscroll),
            )
        } else {
            (0.0, 0.0)
        };
        Self {
            action: event.action(),
            action_index: event.pointer_index(),
            source: event.source(),
            meta_state: event.meta_state(),
            button_state: event.button_state().0,
            canceled: event.flags().0 & FLAG_CANCELED != 0,
            classification: CLASSIFICATION_NONE,
            vscroll,
            hscroll,
            pointers,
        }
    }

    /// Whether Android recognized this as a gesture on a trackpad, such as a
    /// two-finger scroll or a pinch, and reports it with fake fingers. The
    /// classifications of a touchscreen's events have that source instead.
    fn is_trackpad_gesture(&self) -> bool {
        self.source == Source::Mouse && self.classification != CLASSIFICATION_NONE
    }

    /// Whether this is a finger on the touchscreen. A trackpad's presses report a
    /// finger as their tool too, with the mouse's source.
    fn is_touchscreen_finger(&self) -> bool {
        self.source == Source::Touchscreen
            && self.pointers.first().is_some_and(|pointer| {
                !matches!(
                    pointer.tool,
                    ToolType::Mouse | ToolType::Stylus | ToolType::Eraser
                )
            })
    }
}

/// What [`translate_motion`] made of an event.
#[derive(Default)]
pub(crate) struct TranslatedMotion {
    pub(crate) inputs: Vec<PlatformInput>,
    /// Whether the event is consumed. Events fed to gpui's touch recognizer
    /// are, even when they produce no input yet.
    pub(crate) handled: bool,
}

/// Translates `motion`, whose positions Android reports in physical pixels, into
/// the logical pixels gpui expects.
pub(crate) fn translate_motion(
    mut motion: Motion,
    scale_factor: f32,
    state: &mut AndroidWindowState,
) -> TranslatedMotion {
    for pointer in &mut motion.pointers {
        pointer.position = point(
            pointer.position.x / scale_factor,
            pointer.position.y / scale_factor,
        );
    }
    // A touch or a press stops a trackpad scroll's glide, as touching a flinging
    // view does.
    if motion.action == MotionAction::Down {
        state.trackpad_scroll.stop_glide();
    }
    // Its fake fingers aren't presses, so a gesture without a use here does nothing.
    if motion.is_trackpad_gesture() {
        let inputs = if motion.classification == CLASSIFICATION_TWO_FINGER_SWIPE {
            Vec::from_iter(state.trackpad_scroll.translate(&motion))
        } else {
            Vec::new()
        };
        return TranslatedMotion {
            inputs,
            handled: true,
        };
    }
    if motion.is_touchscreen_finger() {
        return TranslatedMotion {
            inputs: state.touch_feed.on_event(&motion, Instant::now()),
            handled: true,
        };
    }
    let inputs = Vec::from_iter(mouse::translate(&motion, &mut state.mouse));
    if matches!(inputs.first(), Some(PlatformInput::MouseMove(_))) {
        crate::cursor::show_after_pointer_moved();
    }
    TranslatedMotion {
        handled: !inputs.is_empty(),
        inputs,
    }
}
