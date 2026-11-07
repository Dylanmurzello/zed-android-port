//! Pointer input: a mouse, a stylus, and a trackpad, whose taps, presses and
//! two-finger taps Android's input stack reports as a pointer's button presses,
//! and whose two-finger swipes it reports as classified scrolls.

use std::time::{Duration, Instant};

use android_activity::input::MotionAction;
use gpui::{
    GestureTuning, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    NavigationDirection, Pixels, PlatformInput, Point, PointerFling, ScrollDelta, ScrollEvent,
    ScrollSource, TouchPhase, point, px,
};

use super::Motion;
use super::keyboard::modifiers_from_meta;

/// `MotionEvent.BUTTON_*`.
const BUTTON_PRIMARY: u32 = 1 << 0;
const BUTTON_SECONDARY: u32 = 1 << 1;
const BUTTON_TERTIARY: u32 = 1 << 2;
const BUTTON_BACK: u32 = 1 << 3;
const BUTTON_FORWARD: u32 = 1 << 4;

/// As Zed's Linux backends do (`DOUBLE_CLICK_INTERVAL` and
/// `DOUBLE_CLICK_DISTANCE` in `gpui_linux`).
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const MULTI_CLICK_DISTANCE: f64 = 5.0;

/// Mouse-wheel detents scroll one "line" each at the framework level
/// (`AXIS_VSCROLL` is about ±1.0 per notch). Desktops move a few lines per
/// notch; 3 keeps the wheel from feeling sluggish.
const WHEEL_LINES_PER_NOTCH: f32 = 3.0;

/// One window's pointer state.
#[derive(Default)]
pub(crate) struct MouseState {
    /// The button pressed by the last `ACTION_DOWN`, until its `ACTION_UP`.
    held: Option<MouseButton>,
    last_click: Option<LastClick>,
}

struct LastClick {
    at: Instant,
    position: Point<Pixels>,
    button: MouseButton,
    count: usize,
}

impl MouseState {
    /// The `click_count` for a new press of `button`: the same button, close
    /// enough in time and place to the last press, continues its run.
    fn next_click_count(&mut self, button: MouseButton, position: Point<Pixels>) -> usize {
        let now = Instant::now();
        let count = match &self.last_click {
            Some(last)
                if last.button == button
                    && now.duration_since(last.at) < MULTI_CLICK_INTERVAL
                    && (position - last.position).magnitude() <= MULTI_CLICK_DISTANCE =>
            {
                last.count + 1
            }
            _ => 1,
        };
        self.last_click = Some(LastClick {
            at: now,
            position,
            button,
            count,
        });
        count
    }

    /// gpui reads a click's count from its `MouseUp`.
    fn click_count(&self) -> usize {
        self.last_click.as_ref().map_or(1, |last| last.count)
    }
}

pub(crate) fn translate(motion: &Motion, state: &mut MouseState) -> Option<PlatformInput> {
    let position = motion.pointers.first()?.position;
    let modifiers = modifiers_from_meta(motion.meta_state);
    match motion.action {
        MotionAction::Down => {
            // A stylus has no button, and One UI can leave it out of a trackpad
            // press.
            let button = button_from_state(motion.button_state).unwrap_or(MouseButton::Left);
            let click_count = state.next_click_count(button, position);
            state.held = Some(button);
            Some(button_down(button, position, modifiers, click_count))
        }
        MotionAction::Up => {
            let button = state.held.take()?;
            Some(button_up(button, position, modifiers, state.click_count()))
        }
        MotionAction::Move => Some(PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: state.held,
            modifiers,
        })),
        // Android sends `ACTION_HOVER_EXIT` before every press too, so it doesn't
        // mean that the pointer left the window.
        MotionAction::HoverEnter | MotionAction::HoverMove => {
            Some(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            }))
        }
        // Released away from every element, so that nothing clicks or drops.
        MotionAction::Cancel => {
            let button = state.held.take()?;
            let nowhere = point(px(-1.), px(-1.));
            Some(button_up(button, nowhere, modifiers, 1))
        }
        MotionAction::Scroll => wheel_scroll(motion.vscroll, motion.hscroll, position, modifiers),
        _ => None,
    }
}

/// A trackpad's two-finger scroll, which Android reports as one fake finger that
/// starts where the pointer is and moves with the scroll. It's a pointer's scroll
/// for gpui, as on iOS, not a touch: a touch could tap, long press, or grab a
/// scrollbar or pane splitter under the pointer. Android leaves the glide after
/// the fingers lift to the app.
pub(crate) struct TrackpadScroll {
    start: Point<Pixels>,
    last: Point<Pixels>,
    fling: PointerFling,
}

impl TrackpadScroll {
    pub(crate) fn new(tuning: GestureTuning) -> Self {
        Self {
            start: Point::default(),
            last: Point::default(),
            fling: PointerFling::new(tuning),
        }
    }

    pub(crate) fn translate(&mut self, motion: &Motion) -> Option<PlatformInput> {
        let position = motion.pointers.first()?.position;
        let (delta, touch_phase) = match motion.action {
            MotionAction::Down => {
                self.start = position;
                self.last = position;
                (Point::default(), TouchPhase::Started)
            }
            MotionAction::Move => {
                let moved = position - self.last;
                self.last = position;
                let delta = invert_pixels(moved);
                self.fling.scrolled(delta);
                (delta, TouchPhase::Moved)
            }
            MotionAction::Up => {
                self.fling.release(self.start);
                // The glide's last step ends the scroll.
                if self.fling.is_gliding() {
                    return None;
                }
                (Point::default(), TouchPhase::Ended)
            }
            MotionAction::Cancel => {
                self.fling.stop();
                (Point::default(), TouchPhase::Cancelled)
            }
            _ => return None,
        };
        Some(PlatformInput::Scroll(ScrollEvent {
            position: self.start,
            delta: ScrollDelta::Pixels(delta),
            modifiers: modifiers_from_meta(motion.meta_state),
            touch_phase,
            source: ScrollSource::Pointer,
        }))
    }

    pub(crate) fn stop_glide(&mut self) {
        self.fling.stop();
    }

    pub(crate) fn is_gliding(&self) -> bool {
        self.fling.is_gliding()
    }

    /// The glide's scroll step for this frame, if one is running.
    pub(crate) fn tick(&mut self) -> Option<ScrollEvent> {
        self.fling.tick()
    }
}

fn invert_pixels(delta: Point<Pixels>) -> Point<Pixels> {
    match crate::ime::invert_scroll_delta(ScrollDelta::Pixels(delta)) {
        ScrollDelta::Pixels(pixels) => pixels,
        ScrollDelta::Lines(_) => delta,
    }
}

/// The gpui button for a `MotionEvent.getButtonState()`, the first pressed one
/// when there are several.
fn button_from_state(state: u32) -> Option<MouseButton> {
    if state & BUTTON_PRIMARY != 0 {
        Some(MouseButton::Left)
    } else if state & BUTTON_TERTIARY != 0 {
        Some(MouseButton::Middle)
    } else if state & BUTTON_SECONDARY != 0 {
        Some(MouseButton::Right)
    } else if state & BUTTON_BACK != 0 {
        Some(MouseButton::Navigate(NavigationDirection::Back))
    } else if state & BUTTON_FORWARD != 0 {
        Some(MouseButton::Navigate(NavigationDirection::Forward))
    } else {
        None
    }
}

/// `first_mouse` is false because there's no window-focus concept on Android:
/// `ClickEvent::first_focus` would report every click as one that only focuses
/// the window, and listeners like the project panel's `on_click` bail on those.
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

pub(crate) fn button_up(
    button: MouseButton,
    position: Point<Pixels>,
    modifiers: Modifiers,
    click_count: usize,
) -> PlatformInput {
    PlatformInput::MouseUp(MouseUpEvent {
        button,
        position,
        modifiers,
        click_count,
    })
}

/// Android's `AXIS_VSCROLL` is +1.0 for "up / forward" (wheel rotated away from
/// the user); gpui's `ScrollDelta::Lines.y` is also +1.0 for "scroll content up"
/// (matches `terminal::mappings::mouse::is_scroll_up` and macOS's raw
/// `scrollingDeltaY`), so it passes through. `AXIS_HSCROLL` is +1.0 for "right",
/// while gpui's `x` is +1.0 for "scroll content left" (Zed's Wayland backend
/// negates horizontal axis values for the same reason).
fn wheel_scroll(
    vscroll: f32,
    hscroll: f32,
    position: Point<Pixels>,
    modifiers: Modifiers,
) -> Option<PlatformInput> {
    if vscroll == 0.0 && hscroll == 0.0 {
        return None;
    }
    Some(PlatformInput::Scroll(ScrollEvent {
        position,
        delta: crate::ime::invert_scroll_delta(ScrollDelta::Lines(point(
            -hscroll * WHEEL_LINES_PER_NOTCH,
            vscroll * WHEEL_LINES_PER_NOTCH,
        ))),
        modifiers,
        touch_phase: TouchPhase::Moved,
        source: ScrollSource::Pointer,
    }))
}

#[cfg(test)]
mod tests {
    use android_activity::input::{MetaState, Source, ToolType};

    use super::*;
    use crate::events::{CLASSIFICATION_NONE, CLASSIFICATION_TWO_FINGER_SWIPE, MotionPointer};

    fn motion(action: MotionAction, button_state: u32, x: f32) -> Motion {
        Motion {
            action,
            action_index: 0,
            source: Source::Mouse,
            meta_state: MetaState(0),
            button_state,
            canceled: false,
            classification: CLASSIFICATION_NONE,
            vscroll: 0.0,
            hscroll: 0.0,
            pointers: vec![MotionPointer {
                id: 0,
                position: point(px(x), px(0.)),
                tool: ToolType::Finger,
            }],
        }
    }

    fn press(state: &mut MouseState, button_state: u32, x: f32) -> (MouseButton, usize) {
        match translate(&motion(MotionAction::Down, button_state, x), state) {
            Some(PlatformInput::MouseDown(down)) => (down.button, down.click_count),
            other => panic!("expected a mouse down, got {other:?}"),
        }
    }

    fn release(state: &mut MouseState, button_state: u32, x: f32) -> MouseUpEvent {
        match translate(&motion(MotionAction::Up, button_state, x), state) {
            Some(PlatformInput::MouseUp(up)) => up,
            other => panic!("expected a mouse up, got {other:?}"),
        }
    }

    #[test]
    fn a_press_carries_its_button_through_to_the_release() {
        let mut state = MouseState::default();
        assert_eq!(
            press(&mut state, BUTTON_SECONDARY, 10.),
            (MouseButton::Right, 1)
        );
        let moved = translate(
            &motion(MotionAction::Move, BUTTON_SECONDARY, 20.),
            &mut state,
        );
        assert!(matches!(
            moved,
            Some(PlatformInput::MouseMove(moved)) if moved.pressed_button == Some(MouseButton::Right)
        ));
        let up = release(&mut state, 0, 20.);
        assert_eq!(up.button, MouseButton::Right);
        assert_eq!(up.position.x, px(20.));
    }

    #[test]
    fn a_press_without_a_button_is_the_left_one() {
        let mut state = MouseState::default();
        assert_eq!(press(&mut state, 0, 10.), (MouseButton::Left, 1));
        assert_eq!(release(&mut state, 0, 10.).button, MouseButton::Left);
    }

    #[test]
    fn repeated_presses_in_place_count_up_and_a_release_reports_the_count() {
        let mut state = MouseState::default();
        assert_eq!(press(&mut state, BUTTON_PRIMARY, 10.).1, 1);
        assert_eq!(release(&mut state, 0, 10.).click_count, 1);
        assert_eq!(press(&mut state, BUTTON_PRIMARY, 12.).1, 2);
        assert_eq!(release(&mut state, 0, 12.).click_count, 2);
        assert_eq!(press(&mut state, BUTTON_PRIMARY, 100.).1, 1);
    }

    #[test]
    fn a_cancelled_press_is_released_away_from_every_element() {
        let mut state = MouseState::default();
        press(&mut state, BUTTON_PRIMARY, 10.);
        match translate(&motion(MotionAction::Cancel, 0, 10.), &mut state) {
            Some(PlatformInput::MouseUp(up)) => assert!(up.position.x < px(0.)),
            other => panic!("expected a mouse up, got {other:?}"),
        }
        assert!(translate(&motion(MotionAction::Cancel, 0, 10.), &mut state).is_none());
    }

    #[test]
    fn hover_moves_with_no_button_and_a_release_without_a_press_is_ignored() {
        let mut state = MouseState::default();
        let hover = translate(&motion(MotionAction::HoverMove, 0, 10.), &mut state);
        assert!(matches!(
            hover,
            Some(PlatformInput::MouseMove(moved)) if moved.pressed_button.is_none()
        ));
        assert!(translate(&motion(MotionAction::Up, 0, 10.), &mut state).is_none());
        assert!(translate(&motion(MotionAction::HoverExit, 0, 10.), &mut state).is_none());
        assert!(translate(&motion(MotionAction::ButtonPress, 1, 10.), &mut state).is_none());
    }

    #[test]
    fn only_a_pointer_event_with_a_classification_is_a_trackpad_gesture() {
        /// `MotionEvent.CLASSIFICATION_DEEP_PRESS`, which is a touchscreen's.
        const CLASSIFICATION_DEEP_PRESS: i32 = 2;
        /// `MotionEvent.CLASSIFICATION_PINCH`.
        const CLASSIFICATION_PINCH: i32 = 5;

        let mut event = motion(MotionAction::Down, 0, 10.);
        assert!(!event.is_trackpad_gesture());
        for classification in [CLASSIFICATION_TWO_FINGER_SWIPE, CLASSIFICATION_PINCH] {
            event.classification = classification;
            assert!(event.is_trackpad_gesture());
        }
        event.source = Source::Touchscreen;
        event.classification = CLASSIFICATION_DEEP_PRESS;
        assert!(!event.is_trackpad_gesture());
    }

    fn scroll(scroll: &mut TrackpadScroll, action: MotionAction, x: f32) -> Option<ScrollEvent> {
        match scroll.translate(&motion(action, 0, x)) {
            Some(PlatformInput::Scroll(event)) => Some(event),
            Some(other) => panic!("expected a scroll, got {other:?}"),
            None => None,
        }
    }

    #[test]
    fn a_two_finger_swipe_scrolls_where_it_started_by_how_far_it_moved() {
        let mut trackpad = TrackpadScroll::new(GestureTuning::default());
        let started = scroll(&mut trackpad, MotionAction::Down, 100.).unwrap();
        assert_eq!(started.touch_phase, TouchPhase::Started);
        assert_eq!(started.position.x, px(100.));
        assert_eq!(started.source, ScrollSource::Pointer);

        for (x, expected) in [(130., 30.), (120., -10.)] {
            let moved = scroll(&mut trackpad, MotionAction::Move, x).unwrap();
            assert_eq!(moved.touch_phase, TouchPhase::Moved);
            assert_eq!(moved.position.x, px(100.));
            assert!(matches!(
                moved.delta,
                ScrollDelta::Pixels(delta) if delta.x == px(expected)
            ));
        }
    }

    #[test]
    fn a_two_finger_swipe_that_stops_before_lifting_ends_without_a_glide() {
        let mut trackpad = TrackpadScroll::new(GestureTuning::default());
        scroll(&mut trackpad, MotionAction::Down, 100.);
        let ended = scroll(&mut trackpad, MotionAction::Up, 100.).unwrap();
        assert_eq!(ended.touch_phase, TouchPhase::Ended);
        assert!(!trackpad.is_gliding());
    }

    #[test]
    fn a_cancelled_two_finger_swipe_scrolls_no_further() {
        let mut trackpad = TrackpadScroll::new(GestureTuning::default());
        scroll(&mut trackpad, MotionAction::Down, 100.);
        let cancelled = scroll(&mut trackpad, MotionAction::Cancel, 100.).unwrap();
        assert_eq!(cancelled.touch_phase, TouchPhase::Cancelled);
        assert!(!trackpad.is_gliding());
        assert!(trackpad.tick().is_none());
    }

    #[test]
    fn the_wheel_scrolls_by_lines() {
        let mut state = MouseState::default();
        let mut wheel = motion(MotionAction::Scroll, 0, 10.);
        wheel.vscroll = 1.0;
        assert!(matches!(
            translate(&wheel, &mut state),
            Some(PlatformInput::Scroll(ScrollEvent {
                delta: ScrollDelta::Lines(lines),
                ..
            })) if lines.y == WHEEL_LINES_PER_NOTCH
        ));
    }
}
