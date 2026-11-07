//! Finger input for gpui.
//!
//! [`TouchFeed`] passes each finger on the touchscreen to gpui as raw
//! `PlatformInput::Touch` events, and gpui's recognizer (`gpui::gestures`) turns
//! them into taps, pans with momentum and long presses. That recognizer follows
//! one touch at a time, so `TouchFeed` resolves two-finger gestures itself: a
//! two-finger tap becomes a secondary click, and a two-finger drag a single touch
//! at the fingers' centroid.
//!
//! Each `AndroidWindowState` owns its own `TouchFeed`, so gestures in different
//! windows can't interfere.

use std::collections::HashMap;
use std::mem;
use std::time::{Duration, Instant};

use android_activity::input::{MotionAction, ToolType};
use gpui::{MouseButton, Pixels, PlatformInput, Point, TouchId, TouchPhase, point};

use crate::events::keyboard::modifiers_from_meta;
use crate::events::mouse::{button_down, button_up};
use crate::events::{Motion, MotionPointer};

/// How far each finger of a two-finger tap may move from where it landed,
/// in touch slops.
const TWO_FINGER_TAP_SLOP_FACTOR: f64 = 2.0;

/// How soon after the second finger lands the last finger must lift for the
/// gesture to be a two-finger tap.
const TWO_FINGER_TAP_TIMEOUT: Duration = Duration::from_millis(500);

/// A touch gpui has seen start and not yet end.
struct OpenTouch {
    id: TouchId,
    start: Point<Pixels>,
    position: Point<Pixels>,
    /// Whether the touch has left the touch slop around its start, which
    /// makes it a pan in gpui's recognizer for good.
    left_slop: bool,
}

#[derive(Default)]
enum TwoFingerGesture {
    /// Fingers reach gpui as they are.
    #[default]
    None,
    /// A second finger landed while the first was still within the touch
    /// slop. The first finger's touch was cancelled, and nothing reaches
    /// gpui until the fingers lift (a secondary click) or move (a pan).
    Pending {
        /// Where the first finger landed: the secondary click goes there.
        anchor: Point<Pixels>,
        second_landed_at: Instant,
        /// The fingers' Android pointer ids and landing points.
        fingers: [(i32, Point<Pixels>); 2],
        one_lifted: bool,
    },
    /// The fingers moved: gpui sees one touch at their centroid.
    Panning { touch: OpenTouch, fingers: [i32; 2] },
    /// The two-finger gesture is over or became something else; the rest
    /// of it doesn't reach gpui.
    Ignored,
}

impl TwoFingerGesture {
    fn follows(&self, pointer_id: i32) -> bool {
        match self {
            Self::Pending { fingers, .. } => fingers.iter().any(|(id, _)| *id == pointer_id),
            Self::Panning { fingers, .. } => fingers.contains(&pointer_id),
            Self::None | Self::Ignored => false,
        }
    }
}

/// Feeds one window's finger input to gpui's touch gesture recognizer.
pub(crate) struct TouchFeed {
    touch_slop: Pixels,
    /// gpui forbids reusing a `TouchId`, and Android numbers pointers from
    /// 0 again in every gesture, so ids come from a counter that only grows.
    next_touch_id: u64,
    /// By Android pointer id.
    open_touches: HashMap<i32, OpenTouch>,
    two_finger: TwoFingerGesture,
}

impl TouchFeed {
    pub(crate) fn new(touch_slop: Pixels) -> Self {
        Self {
            touch_slop,
            next_touch_id: 0,
            open_touches: HashMap::default(),
            two_finger: TwoFingerGesture::None,
        }
    }

    /// `now` is when the event arrived. Batched history samples aren't
    /// replayed: gpui timestamps touches as they arrive, so they would look
    /// simultaneous and distort the fling speed.
    pub(crate) fn on_event(&mut self, event: &Motion, now: Instant) -> Vec<PlatformInput> {
        let mut out = Vec::new();
        match event.action {
            MotionAction::Down => {
                self.cancel_all(&mut out);
                if let Some(pointer) = event.pointers.first()
                    && !is_palm(pointer)
                {
                    self.start(pointer, &mut out);
                }
            }
            MotionAction::PointerDown => {
                self.reject_palms(event, &mut out);
                if let Some(pointer) = event.pointers.get(event.action_index)
                    && !is_palm(pointer)
                {
                    self.pointer_down(pointer, now, &mut out);
                }
            }
            MotionAction::Move => {
                self.reject_palms(event, &mut out);
                self.pointers_moved(event, &mut out);
            }
            MotionAction::PointerUp => {
                self.reject_palms(event, &mut out);
                if let Some(pointer) = event.pointers.get(event.action_index) {
                    self.pointer_up(pointer, event, now, &mut out);
                }
            }
            MotionAction::Up => {
                self.reject_palms(event, &mut out);
                if let Some(pointer) = event.pointers.first() {
                    self.pointer_up(pointer, event, now, &mut out);
                }
                self.cancel_all(&mut out);
            }
            MotionAction::Cancel => self.cancel_all(&mut out),
            _ => {}
        }
        out
    }

    fn start(&mut self, pointer: &MotionPointer, out: &mut Vec<PlatformInput>) {
        if let Some(touch) = self.open_touches.remove(&pointer.id) {
            out.push(touch_input(touch.id, TouchPhase::Cancelled, touch.position));
        }
        let Some(id) = self.allocate_touch_id() else {
            return;
        };
        self.open_touches.insert(
            pointer.id,
            OpenTouch {
                id,
                start: pointer.position,
                position: pointer.position,
                left_slop: false,
            },
        );
        out.push(touch_input(id, TouchPhase::Started, pointer.position));
    }

    fn allocate_touch_id(&mut self) -> Option<TouchId> {
        let id = TouchId(self.next_touch_id);
        let Some(next_touch_id) = self.next_touch_id.checked_add(1) else {
            log::error!("touch: exhausted touch identifiers");
            return None;
        };
        self.next_touch_id = next_touch_id;
        Some(id)
    }

    fn pointer_down(
        &mut self,
        pointer: &MotionPointer,
        now: Instant,
        out: &mut Vec<PlatformInput>,
    ) {
        match self.two_finger {
            TwoFingerGesture::None => {
                if let Some(first_pointer_id) = self.sole_touch_within_slop()
                    && let Some(first) = self.open_touches.remove(&first_pointer_id)
                {
                    out.push(touch_input(first.id, TouchPhase::Cancelled, first.position));
                    self.two_finger = TwoFingerGesture::Pending {
                        anchor: first.start,
                        second_landed_at: now,
                        fingers: [
                            (first_pointer_id, first.start),
                            (pointer.id, pointer.position),
                        ],
                        one_lifted: false,
                    };
                } else {
                    // gpui ignores it while it follows the first finger.
                    self.start(pointer, out);
                }
            }
            TwoFingerGesture::Pending { .. } | TwoFingerGesture::Panning { .. } => {
                self.ignore_two_finger_gesture(out);
            }
            TwoFingerGesture::Ignored => {}
        }
    }

    /// The Android pointer id of the only open touch, if it hasn't left the
    /// touch slop.
    fn sole_touch_within_slop(&self) -> Option<i32> {
        if self.open_touches.len() != 1 {
            return None;
        }
        self.open_touches
            .iter()
            .find(|(_, touch)| !touch.left_slop)
            .map(|(pointer_id, _)| *pointer_id)
    }

    fn pointers_moved(&mut self, event: &Motion, out: &mut Vec<PlatformInput>) {
        let touch_slop = f64::from(self.touch_slop);
        for pointer in &event.pointers {
            let Some(touch) = self.open_touches.get_mut(&pointer.id) else {
                continue;
            };
            if touch.position == pointer.position {
                continue;
            }
            touch.position = pointer.position;
            touch.left_slop |= (pointer.position - touch.start).magnitude() > touch_slop;
            out.push(touch_input(touch.id, TouchPhase::Moved, pointer.position));
        }

        match &mut self.two_finger {
            TwoFingerGesture::Pending {
                fingers,
                one_lifted,
                ..
            } => {
                let tap_slop = touch_slop * TWO_FINGER_TAP_SLOP_FACTOR;
                let moved = fingers.iter().any(|(pointer_id, landing_point)| {
                    event.pointers.iter().any(|pointer| {
                        pointer.id == *pointer_id
                            && (pointer.position - *landing_point).magnitude() > tap_slop
                    })
                });
                if !moved {
                    return;
                }
                let fingers = [fingers[0].0, fingers[1].0];
                let one_lifted = *one_lifted;
                self.two_finger = if one_lifted {
                    TwoFingerGesture::Ignored
                } else {
                    self.start_two_finger_pan(event, fingers, out)
                };
            }
            TwoFingerGesture::Panning { touch, fingers } => {
                if let Some(centroid) = centroid_of(&event.pointers, fingers)
                    && centroid != touch.position
                {
                    touch.position = centroid;
                    out.push(touch_input(touch.id, TouchPhase::Moved, centroid));
                }
            }
            TwoFingerGesture::None | TwoFingerGesture::Ignored => {}
        }
    }

    fn start_two_finger_pan(
        &mut self,
        event: &Motion,
        fingers: [i32; 2],
        out: &mut Vec<PlatformInput>,
    ) -> TwoFingerGesture {
        let Some(centroid) = centroid_of(&event.pointers, &fingers) else {
            return TwoFingerGesture::Ignored;
        };
        let Some(id) = self.allocate_touch_id() else {
            return TwoFingerGesture::Ignored;
        };
        out.push(touch_input(id, TouchPhase::Started, centroid));
        TwoFingerGesture::Panning {
            touch: OpenTouch {
                id,
                start: centroid,
                position: centroid,
                left_slop: false,
            },
            fingers,
        }
    }

    fn pointer_up(
        &mut self,
        pointer: &MotionPointer,
        event: &Motion,
        now: Instant,
        out: &mut Vec<PlatformInput>,
    ) {
        if let Some(touch) = self.open_touches.remove(&pointer.id) {
            let phase = if event.canceled {
                TouchPhase::Cancelled
            } else {
                TouchPhase::Ended
            };
            out.push(touch_input(touch.id, phase, pointer.position));
        }
        if !self.two_finger.follows(pointer.id) {
            return;
        }
        if event.canceled {
            self.ignore_two_finger_gesture(out);
            return;
        }
        match &mut self.two_finger {
            TwoFingerGesture::Pending {
                anchor,
                second_landed_at,
                one_lifted,
                ..
            } => {
                if !*one_lifted {
                    *one_lifted = true;
                    return;
                }
                let anchor = *anchor;
                let tapped =
                    now.saturating_duration_since(*second_landed_at) <= TWO_FINGER_TAP_TIMEOUT;
                self.two_finger = TwoFingerGesture::Ignored;
                if tapped {
                    let modifiers = modifiers_from_meta(event.meta_state);
                    out.push(button_down(MouseButton::Right, anchor, modifiers, 1));
                    out.push(button_up(MouseButton::Right, anchor, modifiers, 1));
                }
            }
            TwoFingerGesture::Panning { touch, .. } => {
                out.push(touch_input(touch.id, TouchPhase::Ended, touch.position));
                self.two_finger = TwoFingerGesture::Ignored;
            }
            TwoFingerGesture::None | TwoFingerGesture::Ignored => {}
        }
    }

    /// Palms reach gpui as cancelled touches and end a two-finger gesture
    /// they're part of.
    fn reject_palms(&mut self, event: &Motion, out: &mut Vec<PlatformInput>) {
        for pointer in event.pointers.iter().filter(|pointer| is_palm(pointer)) {
            if let Some(touch) = self.open_touches.remove(&pointer.id) {
                out.push(touch_input(
                    touch.id,
                    TouchPhase::Cancelled,
                    pointer.position,
                ));
            }
            if self.two_finger.follows(pointer.id) {
                self.ignore_two_finger_gesture(out);
            }
        }
    }

    fn ignore_two_finger_gesture(&mut self, out: &mut Vec<PlatformInput>) {
        if let TwoFingerGesture::Panning { touch, .. } =
            mem::replace(&mut self.two_finger, TwoFingerGesture::Ignored)
        {
            out.push(touch_input(touch.id, TouchPhase::Cancelled, touch.position));
        }
    }

    fn cancel_all(&mut self, out: &mut Vec<PlatformInput>) {
        for (_, touch) in self.open_touches.drain() {
            out.push(touch_input(touch.id, TouchPhase::Cancelled, touch.position));
        }
        if let TwoFingerGesture::Panning { touch, .. } = mem::take(&mut self.two_finger) {
            out.push(touch_input(touch.id, TouchPhase::Cancelled, touch.position));
        }
    }
}

/// The centroid of the given pointers, if the event has all of them.
fn centroid_of(pointers: &[MotionPointer], pointer_ids: &[i32]) -> Option<Point<Pixels>> {
    let mut sum = Point::<Pixels>::default();
    for pointer_id in pointer_ids {
        let pointer = pointers.iter().find(|pointer| pointer.id == *pointer_id)?;
        sum = sum + pointer.position;
    }
    let count = pointer_ids.len() as f32;
    (count > 0.0).then(|| point(sum.x / count, sum.y / count))
}

/// `MotionEvent.TOOL_TYPE_PALM`: the system decided this contact is a palm resting
/// on the screen.
fn is_palm(pointer: &MotionPointer) -> bool {
    pointer.tool == ToolType::Palm
}

fn touch_input(id: TouchId, phase: TouchPhase, position: Point<Pixels>) -> PlatformInput {
    PlatformInput::Touch(gpui::TouchEvent {
        id,
        phase,
        position,
        predicted_position: None,
        force: None,
    })
}

#[cfg(test)]
mod tests {
    use android_activity::input::{MetaState, Source};
    use gpui::px;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum Output {
        Touch(u64, TouchPhase, (f32, f32)),
        SecondaryDown((f32, f32)),
        SecondaryUp((f32, f32)),
    }

    fn outputs(inputs: Vec<PlatformInput>) -> Vec<Output> {
        let coordinates = |position: Point<Pixels>| (f32::from(position.x), f32::from(position.y));
        inputs
            .into_iter()
            .map(|input| match input {
                PlatformInput::Touch(touch) => {
                    Output::Touch(touch.id.0, touch.phase, coordinates(touch.position))
                }
                PlatformInput::MouseDown(down) if down.button == MouseButton::Right => {
                    Output::SecondaryDown(coordinates(down.position))
                }
                PlatformInput::MouseUp(up) if up.button == MouseButton::Right => {
                    Output::SecondaryUp(coordinates(up.position))
                }
                other => panic!("unexpected input {other:?}"),
            })
            .collect()
    }

    fn finger(id: i32, x: f32, y: f32) -> MotionPointer {
        MotionPointer {
            id,
            position: point(px(x), px(y)),
            tool: ToolType::Finger,
        }
    }

    fn palm(id: i32, x: f32, y: f32) -> MotionPointer {
        MotionPointer {
            tool: ToolType::Palm,
            ..finger(id, x, y)
        }
    }

    fn event_at(action: MotionAction, action_index: usize, pointers: Vec<MotionPointer>) -> Motion {
        Motion {
            action,
            action_index,
            source: Source::Touchscreen,
            meta_state: MetaState(0),
            button_state: 0,
            canceled: false,
            classification: crate::events::CLASSIFICATION_NONE,
            vscroll: 0.0,
            hscroll: 0.0,
            pointers,
        }
    }

    fn event(action: MotionAction, pointers: Vec<MotionPointer>) -> Motion {
        event_at(action, 0, pointers)
    }

    fn feed() -> TouchFeed {
        TouchFeed::new(px(8.0))
    }

    #[test]
    fn touch_ids_are_never_reused() {
        let mut feed = feed();
        let now = Instant::now();
        let down = event(MotionAction::Down, vec![finger(0, 10.0, 10.0)]);
        let up = event(MotionAction::Up, vec![finger(0, 10.0, 10.0)]);
        assert_eq!(
            outputs(feed.on_event(&down, now)),
            [Output::Touch(0, TouchPhase::Started, (10.0, 10.0))]
        );
        assert_eq!(
            outputs(feed.on_event(&up, now)),
            [Output::Touch(0, TouchPhase::Ended, (10.0, 10.0))]
        );
        assert_eq!(
            outputs(feed.on_event(&down, now)),
            [Output::Touch(1, TouchPhase::Started, (10.0, 10.0))]
        );
    }

    #[test]
    fn down_cancels_a_touch_left_open() {
        let mut feed = feed();
        let now = Instant::now();
        feed.on_event(&event(MotionAction::Down, vec![finger(0, 10.0, 10.0)]), now);
        assert_eq!(
            outputs(feed.on_event(&event(MotionAction::Down, vec![finger(0, 30.0, 30.0)]), now)),
            [
                Output::Touch(0, TouchPhase::Cancelled, (10.0, 10.0)),
                Output::Touch(1, TouchPhase::Started, (30.0, 30.0)),
            ]
        );
    }

    #[test]
    fn canceled_lift_and_palm_cancel_the_touch() {
        let mut feed = feed();
        let now = Instant::now();
        feed.on_event(&event(MotionAction::Down, vec![finger(0, 10.0, 10.0)]), now);
        let mut up = event(MotionAction::Up, vec![finger(0, 10.0, 10.0)]);
        up.canceled = true;
        assert_eq!(
            outputs(feed.on_event(&up, now)),
            [Output::Touch(0, TouchPhase::Cancelled, (10.0, 10.0))]
        );

        feed.on_event(&event(MotionAction::Down, vec![finger(0, 10.0, 10.0)]), now);
        let moved = event(MotionAction::Move, vec![palm(0, 12.0, 10.0)]);
        assert_eq!(
            outputs(feed.on_event(&moved, now)),
            [Output::Touch(1, TouchPhase::Cancelled, (12.0, 10.0))]
        );
        let up = event(MotionAction::Up, vec![palm(0, 12.0, 10.0)]);
        assert!(outputs(feed.on_event(&up, now)).is_empty());
    }

    #[test]
    fn two_finger_tap_clicks_secondary_on_lift() {
        let mut feed = feed();
        let start = Instant::now();
        let second_landed = start + Duration::from_millis(50);
        feed.on_event(
            &event(MotionAction::Down, vec![finger(0, 100.0, 100.0)]),
            start,
        );
        let second_down = event_at(
            MotionAction::PointerDown,
            1,
            vec![finger(0, 101.0, 100.0), finger(1, 200.0, 100.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&second_down, second_landed)),
            [Output::Touch(0, TouchPhase::Cancelled, (100.0, 100.0))]
        );
        let jitter = event(
            MotionAction::Move,
            vec![finger(0, 104.0, 100.0), finger(1, 203.0, 100.0)],
        );
        assert!(outputs(feed.on_event(&jitter, second_landed)).is_empty());
        let first_up = event_at(
            MotionAction::PointerUp,
            1,
            vec![finger(0, 104.0, 100.0), finger(1, 203.0, 100.0)],
        );
        assert!(
            outputs(feed.on_event(&first_up, second_landed + Duration::from_millis(150)))
                .is_empty()
        );
        let last_up = event(MotionAction::Up, vec![finger(0, 104.0, 100.0)]);
        assert_eq!(
            outputs(feed.on_event(&last_up, second_landed + Duration::from_millis(200))),
            [
                Output::SecondaryDown((100.0, 100.0)),
                Output::SecondaryUp((100.0, 100.0)),
            ]
        );
    }

    #[test]
    fn slow_two_finger_tap_does_nothing() {
        let mut feed = feed();
        let start = Instant::now();
        feed.on_event(
            &event(MotionAction::Down, vec![finger(0, 100.0, 100.0)]),
            start,
        );
        feed.on_event(
            &event_at(
                MotionAction::PointerDown,
                1,
                vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
            ),
            start,
        );
        let late = start + TWO_FINGER_TAP_TIMEOUT + Duration::from_millis(1);
        let first_up = event_at(
            MotionAction::PointerUp,
            1,
            vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
        );
        assert!(outputs(feed.on_event(&first_up, late)).is_empty());
        let last_up = event(MotionAction::Up, vec![finger(0, 100.0, 100.0)]);
        assert!(outputs(feed.on_event(&last_up, late)).is_empty());
    }

    #[test]
    fn two_finger_drag_pans_at_the_centroid() {
        let mut feed = feed();
        let now = Instant::now();
        feed.on_event(
            &event(MotionAction::Down, vec![finger(0, 100.0, 100.0)]),
            now,
        );
        feed.on_event(
            &event_at(
                MotionAction::PointerDown,
                1,
                vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
            ),
            now,
        );
        let drag = event(
            MotionAction::Move,
            vec![finger(0, 100.0, 130.0), finger(1, 200.0, 130.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&drag, now)),
            [Output::Touch(1, TouchPhase::Started, (150.0, 130.0))]
        );
        let drag = event(
            MotionAction::Move,
            vec![finger(0, 100.0, 140.0), finger(1, 200.0, 140.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&drag, now)),
            [Output::Touch(1, TouchPhase::Moved, (150.0, 140.0))]
        );
        let first_up = event_at(
            MotionAction::PointerUp,
            0,
            vec![finger(0, 100.0, 140.0), finger(1, 200.0, 140.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&first_up, now)),
            [Output::Touch(1, TouchPhase::Ended, (150.0, 140.0))]
        );
        let drag = event(MotionAction::Move, vec![finger(1, 200.0, 160.0)]);
        assert!(outputs(feed.on_event(&drag, now)).is_empty());
        let last_up = event(MotionAction::Up, vec![finger(1, 200.0, 160.0)]);
        assert!(outputs(feed.on_event(&last_up, now)).is_empty());
    }

    #[test]
    fn third_finger_cancels_the_pan() {
        let mut feed = feed();
        let now = Instant::now();
        feed.on_event(
            &event(MotionAction::Down, vec![finger(0, 100.0, 100.0)]),
            now,
        );
        feed.on_event(
            &event_at(
                MotionAction::PointerDown,
                1,
                vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
            ),
            now,
        );
        feed.on_event(
            &event(
                MotionAction::Move,
                vec![finger(0, 100.0, 130.0), finger(1, 200.0, 130.0)],
            ),
            now,
        );
        let third_down = event_at(
            MotionAction::PointerDown,
            2,
            vec![
                finger(0, 100.0, 130.0),
                finger(1, 200.0, 130.0),
                finger(2, 300.0, 130.0),
            ],
        );
        assert_eq!(
            outputs(feed.on_event(&third_down, now)),
            [Output::Touch(1, TouchPhase::Cancelled, (150.0, 130.0))]
        );
        let drag = event(
            MotionAction::Move,
            vec![
                finger(0, 100.0, 160.0),
                finger(1, 200.0, 160.0),
                finger(2, 300.0, 160.0),
            ],
        );
        assert!(outputs(feed.on_event(&drag, now)).is_empty());
    }

    #[test]
    fn second_finger_after_the_first_left_the_slop_is_forwarded() {
        let mut feed = feed();
        let now = Instant::now();
        feed.on_event(
            &event(MotionAction::Down, vec![finger(0, 100.0, 100.0)]),
            now,
        );
        let drag = event(MotionAction::Move, vec![finger(0, 100.0, 120.0)]);
        assert_eq!(
            outputs(feed.on_event(&drag, now)),
            [Output::Touch(0, TouchPhase::Moved, (100.0, 120.0))]
        );
        let second_down = event_at(
            MotionAction::PointerDown,
            1,
            vec![finger(0, 100.0, 120.0), finger(1, 200.0, 100.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&second_down, now)),
            [Output::Touch(1, TouchPhase::Started, (200.0, 100.0))]
        );
        let first_up = event_at(
            MotionAction::PointerUp,
            0,
            vec![finger(0, 100.0, 120.0), finger(1, 200.0, 100.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&first_up, now)),
            [Output::Touch(0, TouchPhase::Ended, (100.0, 120.0))]
        );
        let last_up = event(MotionAction::Up, vec![finger(1, 200.0, 100.0)]);
        assert_eq!(
            outputs(feed.on_event(&last_up, now)),
            [Output::Touch(1, TouchPhase::Ended, (200.0, 100.0))]
        );
    }
}
