//! First-class native touch gesture state machine.
//!
//! Touch on Android (`SOURCE_TOUCHSCREEN` + `ToolType::Finger`) arrives
//! through android-activity's NDK native input queue and is dispatched
//! from `platform.rs`'s `InputEvent::MotionEvent` handler. Standard
//! Android app code would lean on Java's `GestureDetector` for tap /
//! scroll / longpress classification, but `GameActivity` removes the
//! `AInputQueue` and the framework gesture helpers along with it, so we
//! hand-roll the SM here. Same approach as Unity / Unreal / Godot and
//! Google's own NDK `gestureDetector.h` sample.
//!
//! Peer of `captured_pointer.rs` (captured-trackpad SM). Mouse, stylus,
//! and the captured-trackpad path still flow through their existing
//! dispatchers; only the `InputSource::Finger` branch routes here.
//!
//! Gesture vocabulary (Phase 1; longpress-selection lands in Phase 4):
//! - 1-finger tap → `MouseDown(Left)` + `MouseUp(Left, count=1+)`
//! - 1-finger drag → cancel pending Left + `Scroll(...)` per frame
//! - 1-finger longpress → keeps `Left` held (no synthetic right-click;
//!   collides with text selection)
//! - 2-finger tap → cancel pending Left + synthesize Right click sequence
//! - 2-finger drag → `Scroll` from centroid relative-delta
//!
//! State ownership: `TouchState` is a field on `AndroidWindowState`.
//! Both primary and extra-window surfaces own their own `TouchState`,
//! so gestures in different windows can never interfere. No
//! thread-locals, no global mutable state — lifetime is bound to the
//! window struct.
//!
//! Input normalization: the SM operates on a `TouchEvent` (this
//! module's own type), not Android's `MotionEvent`. Primary surface
//! events arrive as NDK `MotionEvent`; extras as JNI-marshaled
//! `(action_masked, positions, ...)` tuples. Both build a `TouchEvent`
//! via the local helpers and feed it to the same `on_event` SM. One
//! state machine, two sources, fully testable in isolation.
//!
//! With `android_input.legacy_touch` off, finger gestures skip the SM:
//! [`TouchFeed`] passes them to gpui's own gesture recognizer as raw
//! `PlatformInput::Touch` events instead.

use std::collections::HashMap;
use std::mem;
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use android_activity::input::{MetaState, MotionAction, MotionEvent, ToolType};
use gpui::{
    MouseButton, MouseDownEvent, MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta,
    ScrollEvent, ScrollSource, TouchId, TouchPhase, point, px,
};

use crate::events::keyboard;
use crate::events::{
    JAVA_ACTION_CANCEL, JAVA_ACTION_DOWN, JAVA_ACTION_MOVE, JAVA_ACTION_POINTER_DOWN,
    JAVA_ACTION_POINTER_UP, JAVA_ACTION_UP, JAVA_FLAG_CANCELED, JAVA_TOOL_TYPE_FINGER,
    JAVA_TOOL_TYPE_PALM,
};
use crate::events::{MotionInputs, TranslatedMotion};
use crate::window::AndroidWindowState;

/// Logical-pixel motion threshold for committing to scroll instead of
/// tap. Matches Android's default `ViewConfiguration.getScaledTouchSlop()`
/// (8dp on most devices) so the feel aligns with native apps. Below this,
/// a moving finger is still a tap candidate.
const DRAG_THRESHOLD_PX: f64 = 8.0;

/// Time the primary finger must stay still (below `DRAG_THRESHOLD_PX`)
/// before we commit to a long-press → word-select transition. Matches
/// Android's `ViewConfiguration.getLongPressTimeout()` default (500ms).
/// At long-press fire the SM cancels the pending `MouseDown(Left,
/// count=1)` and emits `MouseDown(Left, count=2)` so the editor word-
/// selects at the anchor; subsequent moves emit `MouseMove(Left held)`
/// to extend the selection.
const LONG_PRESS_THRESHOLD: Duration = Duration::from_millis(500);

/// Maximum delay between the primary finger landing and a second finger
/// landing for the gesture to count as a 2-finger tap (right-click).
/// Generous (longer than `LONG_PRESS_THRESHOLD`) so a user can
/// long-press to select, then drop a second finger to trigger the
/// context menu on the selected word — Moonlight-style hold-and-tap.
const TWO_FINGER_TAP_WINDOW: Duration = Duration::from_millis(800);

/// Maximum logical-pixel distance between the primary's anchor and where
/// the second finger lands for the gesture to count as a 2-finger tap.
/// Bumped from a former 12px (which never fired in practice — index +
/// middle finger natural spread on a tablet is ~20-40mm = 200-400 logical
/// pixels at typical density). 250px covers normal two-finger landings
/// without false-positiving on clearly-distant two-finger gestures
/// (those resolve as multi-finger scroll instead).
const TWO_FINGER_TAP_SLOP_PX: f64 = 250.0;

/// Normalized touch input the SM consumes. Built from either an Android
/// `MotionEvent` (primary surface) or the JNI-marshaled fields
/// (extras). All coordinates are *logical pixels*; the source-specific
/// builders divide by `scale_factor`.
pub(crate) struct TouchEvent {
    pub action: TouchAction,
    pub modifiers: gpui::Modifiers,
    pub pointers: Vec<TouchPointer>,
    /// `MotionEvent.FLAG_CANCELED`: the pointer going up was cancelled.
    pub canceled: bool,
}

/// Action the SM dispatches on. Java `MotionEvent.ACTION_*` collapsed
/// to the touch-relevant subset; the non-touch actions (HOVER_*,
/// SCROLL, BUTTON_*) never reach the touch SM and aren't represented.
pub(crate) enum TouchAction {
    Down,
    /// Additional finger landed. `index` is the position of the newly-
    /// added pointer in [`TouchEvent::pointers`].
    PointerDown {
        index: usize,
    },
    Move,
    /// A non-last finger lifted. `index` is the position of the lifting
    /// pointer in [`TouchEvent::pointers`] (the pointer is still
    /// present in the array per Android's POINTER_UP semantics).
    PointerUp {
        index: usize,
    },
    Up,
    Cancel,
}

/// One pointer's state at the moment this event was generated.
/// Coordinates are logical pixels. `history` carries any batched
/// samples reported between the previous MOVE and this one, in
/// chronological order; the final position is the implicit
/// most-recent sample. Empty when the source doesn't carry history
/// (e.g. extras-window JNI marshaling doesn't plumb it through yet).
pub(crate) struct TouchPointer {
    pub id: i32,
    pub pos: Point<Pixels>,
    pub history: Vec<Point<Pixels>>,
    /// `MotionEvent.TOOL_TYPE_PALM`: the system decided this contact is a
    /// palm resting on the screen.
    pub palm: bool,
}

/// Which recognizer a finger gesture goes to. Decided when the first
/// finger lands and kept until the gesture ends, so changing
/// `android_input.legacy_touch` mid-gesture can't split a gesture
/// between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FingerRoute {
    /// Zdroid's finger state machine, [`TouchState`].
    #[default]
    Legacy,
    /// gpui's touch gesture recognizer, through [`TouchFeed`].
    Gestures,
}

pub(crate) fn dispatch_primary(
    state: &mut AndroidWindowState,
    event: &MotionEvent,
    scale_factor: f32,
) -> TranslatedMotion {
    let Some(touch_event) = build_from_motion_event(event, scale_factor) else {
        return TranslatedMotion::default();
    };
    dispatch(state, &touch_event, true, scale_factor)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_extra(
    state: &mut AndroidWindowState,
    _window_id: u64,
    action_masked: i32,
    action_index: i32,
    meta_state: i32,
    _button_state: i32,
    flags: i32,
    _vscroll: f32,
    _hscroll: f32,
    positions: &[(f32, f32, i32)],
    tool_types: &[i32],
    scale_factor: f32,
) -> MotionInputs {
    let Some(touch_event) = build_from_extra_fields(
        action_masked,
        action_index,
        meta_state,
        flags,
        positions,
        tool_types,
        scale_factor,
    ) else {
        return MotionInputs::new();
    };
    // Mouse and stylus input reaches this path in extra windows too.
    let finger = tool_types.first() == Some(&JAVA_TOOL_TYPE_FINGER);
    dispatch(state, &touch_event, finger, scale_factor).inputs
}

/// Sends a touch event to the virtual trackpad, the finger state machine or
/// gpui's gesture recognizer. `finger` says whether a gesture starting with
/// this event may go to gpui's recognizer.
fn dispatch(
    state: &mut AndroidWindowState,
    touch_event: &TouchEvent,
    finger: bool,
    scale_factor: f32,
) -> TranslatedMotion {
    if crate::ime::trackpad_mode_enabled() {
        let mut inputs = state.touch_feed.abandon();
        let android_app = state.android_app.clone();
        let extra_window_id = state.extra_window_id;
        let bounds = state.bounds.size;
        inputs.extend(state.trackpad_touch.on_event(
            touch_event,
            &android_app,
            scale_factor,
            extra_window_id,
            bounds,
        ));
        return inputs.into();
    }
    if matches!(touch_event.action, TouchAction::Down) {
        state.finger_route = if finger && !crate::ime::legacy_touch_enabled() {
            FingerRoute::Gestures
        } else {
            FingerRoute::Legacy
        };
    }
    match state.finger_route {
        FingerRoute::Gestures => TranslatedMotion {
            inputs: state.touch_feed.on_event(touch_event, Instant::now()),
            handled: true,
        },
        FingerRoute::Legacy => {
            let mut inputs = state.touch_feed.abandon();
            let drag_capture = state.drag_active.load(std::sync::atomic::Ordering::Relaxed);
            inputs.extend(state.touch.on_event(touch_event, drag_capture));
            inputs.into()
        }
    }
}

/// Build a [`TouchEvent`] from an Android NDK `MotionEvent`. Carries
/// per-pointer historical samples so the SM can iterate them and not
/// drop motion on batched 120Hz events.
fn build_from_motion_event(event: &MotionEvent, scale_factor: f32) -> Option<TouchEvent> {
    let pcount = event.pointer_count();
    if pcount == 0 {
        return None;
    }
    let action = match event.action() {
        MotionAction::Down => TouchAction::Down,
        MotionAction::PointerDown => TouchAction::PointerDown {
            index: event.pointer_index(),
        },
        MotionAction::Move => TouchAction::Move,
        MotionAction::PointerUp => TouchAction::PointerUp {
            index: event.pointer_index(),
        },
        MotionAction::Up => TouchAction::Up,
        MotionAction::Cancel => TouchAction::Cancel,
        _ => return None,
    };
    let modifiers = keyboard::modifiers_from_meta(event.meta_state());
    let mut pointers = Vec::with_capacity(pcount);
    for i in 0..pcount {
        let p = event.pointer_at_index(i);
        let pos = point(px(p.x() / scale_factor), px(p.y() / scale_factor));
        let history: Vec<Point<Pixels>> = p
            .history()
            .map(|h| point(px(h.x() / scale_factor), px(h.y() / scale_factor)))
            .collect();
        pointers.push(TouchPointer {
            id: p.pointer_id(),
            pos,
            history,
            palm: p.tool_type() == ToolType::Palm,
        });
    }
    Some(TouchEvent {
        action,
        modifiers,
        pointers,
        canceled: event.flags().0 & JAVA_FLAG_CANCELED as u32 != 0,
    })
}

/// Build a [`TouchEvent`] from the JNI-marshaled fields the extras
/// pipeline sends. Historical samples aren't plumbed across the JNI
/// boundary yet, so `TouchPointer::history` is always empty — extras
/// MOVE events are single-sample.
fn build_from_extra_fields(
    action_masked: i32,
    action_index: i32,
    meta_state: i32,
    flags: i32,
    positions: &[(f32, f32, i32)],
    tool_types: &[i32],
    scale_factor: f32,
) -> Option<TouchEvent> {
    if positions.is_empty() {
        return None;
    }
    let index = action_index.max(0) as usize;
    let action = match action_masked {
        JAVA_ACTION_DOWN => TouchAction::Down,
        JAVA_ACTION_POINTER_DOWN => TouchAction::PointerDown { index },
        JAVA_ACTION_MOVE => TouchAction::Move,
        JAVA_ACTION_POINTER_UP => TouchAction::PointerUp { index },
        JAVA_ACTION_UP => TouchAction::Up,
        JAVA_ACTION_CANCEL => TouchAction::Cancel,
        _ => return None,
    };
    let modifiers = keyboard::modifiers_from_meta(MetaState(meta_state as u32));
    let pointers = positions
        .iter()
        .enumerate()
        .map(|(index, (x, y, id))| TouchPointer {
            id: *id,
            pos: point(px(x / scale_factor), px(y / scale_factor)),
            history: Vec::new(),
            palm: tool_types.get(index) == Some(&JAVA_TOOL_TYPE_PALM),
        })
        .collect();
    Some(TouchEvent {
        action,
        modifiers,
        pointers,
        canceled: flags & JAVA_FLAG_CANCELED != 0,
    })
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum GesturePhase {
    #[default]
    Idle,
    /// One finger down; not yet decided tap vs drag vs long-press.
    SingleFingerDown,
    /// One finger; the platform reports an element is driving a direct-
    /// manipulation drag (e.g. scrollbar thumb). SM passes the gesture
    /// through as `MouseMove(Left held)` for the dragging element to
    /// consume; no scroll synthesis, no long-press transition. Cleared
    /// on lift / cancel.
    DragPassthrough,
    /// One finger; motion past `DRAG_THRESHOLD_PX`, emitting `Scroll`.
    SingleFingerScroll,
    /// One finger held past `LONG_PRESS_THRESHOLD` with motion still
    /// below `DRAG_THRESHOLD_PX`. `MouseDown(Left, count=2)` already
    /// emitted at the anchor; subsequent moves emit `MouseMove(Left
    /// held)` to extend the selection. `Up` emits the matching
    /// `MouseUp(Left, count=2)` to lock the selection. The Android
    /// equivalent of Chrome / EditText's "long-press to select word,
    /// drag to extend" behavior.
    LongPressSelection,
    /// 2+ fingers down within the 2-finger-tap window. Not yet decided
    /// 2-finger-tap (→ right click) vs 2-finger-scroll (→ scroll).
    MultiFingerDown,
    /// 2+ fingers; motion past `DRAG_THRESHOLD_PX`, emitting `Scroll`.
    MultiFingerScroll,
    /// 2-finger tap already resolved as Right click. Waiting for all
    /// fingers to lift before returning to `Idle`.
    RightClickResolved,
}

/// Per-pointer state for the active gesture. Keyed by pointer-ID
/// (stable across the gesture — survives the pointer-array reordering
/// Android does when a non-last finger lifts).
#[derive(Debug, Clone)]
struct PointerState {
    down_time: Instant,
    down_pos: Point<Pixels>,
    last_pos: Point<Pixels>,
    /// Sum of per-MOVE absolute deltas in logical pixels since
    /// `down_pos`. Crosses `DRAG_THRESHOLD_PX` to commit the gesture
    /// to scroll instead of tap.
    accumulated_motion: f64,
}

#[derive(Default)]
pub(crate) struct TouchState {
    pointers: HashMap<i32, PointerState>,
    phase: GesturePhase,
    /// Last centroid in logical pixels. Scroll deltas are computed as
    /// `cur_centroid - scroll_centroid` directly (no scale-factor divide;
    /// input is already in logical units).
    scroll_centroid: Option<Point<Pixels>>,
}

/// Average of all active pointers' logical-pixel positions. Returns
/// `Point::default()` (zero) when called with no pointers; the SM only
/// invokes this in branches that already verified `pointers.len() >= 2`.
fn centroid(pointers: &[TouchPointer]) -> Point<Pixels> {
    let n = pointers.len();
    if n == 0 {
        return Point::default();
    }
    let mut sx = px(0.0);
    let mut sy = px(0.0);
    for p in pointers {
        sx += p.pos.x;
        sy += p.pos.y;
    }
    point(sx / n as f32, sy / n as f32)
}

impl TouchState {
    /// `drag_capture` is the snapshot of `AndroidWindowState::drag_active`
    /// at dispatch time. When set, the SM treats single-finger motion
    /// as a direct-manipulation drag (passes through as `MouseMove(Left
    /// held)`) instead of converting to scroll. Scrollbar thumbs and
    /// other graspable widgets flip the flag via
    /// `Window::set_drag_active` to opt in.
    pub(crate) fn on_event(&mut self, event: &TouchEvent, drag_capture: bool) -> MotionInputs {
        let mut out = MotionInputs::new();
        if event.pointers.is_empty() {
            return out;
        }
        let modifiers = event.modifiers;

        match event.action {
            TouchAction::Down => {
                // First finger lands. Discard any stale state from a
                // previous gesture the OS didn't bother to Cancel, then
                // latch this pointer. Always emit `click_count=1` —
                // tap-tap-counting is dropped on touch; word-select
                // comes from long-press, not tap repetition.
                self.reset();
                let primary = &event.pointers[0];
                let position = primary.pos;
                self.pointers.insert(
                    primary.id,
                    PointerState {
                        down_time: Instant::now(),
                        down_pos: position,
                        last_pos: position,
                        accumulated_motion: 0.0,
                    },
                );
                self.phase = GesturePhase::SingleFingerDown;
                out.push(PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                }));
            }

            TouchAction::PointerDown { index } => {
                // Additional finger landed. Three sub-cases:
                //   1. Second finger lands fast + near primary's anchor
                //      → 2-finger tap (right-click). Emit cancel(Left)
                //      + down(Right) + up(Right), enter
                //      `RightClickResolved`.
                //   2. Second finger lands but outside the tap window
                //      or away from anchor → committed to multi-finger
                //      gesture (scroll-pending). Cancel any in-flight
                //      Left and enter `MultiFingerDown`.
                //   3. Already past `SingleFingerScroll` (one-finger
                //      drag committed to scroll) → just track the new
                //      pointer and stay in multi-finger; centroid will
                //      recompute on next MOVE.
                if index >= event.pointers.len() {
                    return out;
                }
                let new_p = &event.pointers[index];
                let new_pos = new_p.pos;
                self.pointers.insert(
                    new_p.id,
                    PointerState {
                        down_time: Instant::now(),
                        down_pos: new_pos,
                        last_pos: new_pos,
                        accumulated_motion: 0.0,
                    },
                );

                let primary = &event.pointers[0];
                let primary_id = primary.id;
                let primary_pos = primary.pos;

                // Two contexts qualify for right-click:
                //   - `SingleFingerDown` within `TWO_FINGER_TAP_WINDOW`
                //     and `TWO_FINGER_TAP_SLOP_PX` of the primary's
                //     anchor (the simultaneous two-finger tap).
                //   - `LongPressSelection` regardless of timing/slop —
                //     the user has already committed to a held-finger
                //     gesture via long-press; the 2nd finger landing
                //     is Moonlight-style "hold-and-tap" and should fire
                //     the context menu on the selected word.
                let qualifies_as_two_finger_tap = match self.phase {
                    GesturePhase::SingleFingerDown => {
                        self.pointers.get(&primary_id).is_some_and(|s| {
                            s.down_time.elapsed() < TWO_FINGER_TAP_WINDOW
                                && (new_pos - s.down_pos).magnitude() <= TWO_FINGER_TAP_SLOP_PX
                        })
                    }
                    GesturePhase::LongPressSelection => true,
                    _ => false,
                };

                if qualifies_as_two_finger_tap {
                    // The primary's anchor is where we want the
                    // right-click to land — that's where the user's
                    // attention is, not where the second finger
                    // happened to touch.
                    let anchor = self
                        .pointers
                        .get(&primary_id)
                        .map(|s| s.down_pos)
                        .unwrap_or(primary_pos);
                    // If we were in `LongPressSelection`, close the
                    // long-press cleanly so the selection locks before
                    // the context-menu emission. The pending
                    // `MouseDown(Left, count=2)` was emitted at
                    // long-press fire; emit its matching `Up` here.
                    let cancel_count = if matches!(self.phase, GesturePhase::LongPressSelection) {
                        2
                    } else {
                        0
                    };
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: anchor,
                        modifiers,
                        click_count: cancel_count,
                    }));
                    out.push(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Right,
                        position: anchor,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }));
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Right,
                        position: anchor,
                        modifiers,
                        click_count: 1,
                    }));
                    self.phase = GesturePhase::RightClickResolved;
                    return out;
                }

                // Not a 2-finger tap. Cancel any in-flight Left from the
                // original DOWN so the editor doesn't see a stuck button
                // while we proceed into scroll classification. It releases
                // where it went down: with no move in between, the editor
                // takes a release elsewhere for a drag that never started.
                if matches!(self.phase, GesturePhase::SingleFingerDown) {
                    let anchor = self
                        .pointers
                        .get(&primary_id)
                        .map(|s| s.down_pos)
                        .unwrap_or(primary_pos);
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: anchor,
                        modifiers,
                        click_count: 0,
                    }));
                }
                if matches!(
                    self.phase,
                    GesturePhase::SingleFingerDown | GesturePhase::SingleFingerScroll
                ) {
                    self.phase = GesturePhase::MultiFingerDown;
                    // Drop any prior centroid; the MOVE handler will
                    // recompute against the new pointer set.
                    self.scroll_centroid = None;
                }
            }

            TouchAction::Move => {
                // Update per-pointer state for ALL active pointers,
                // iterating historical samples so 120Hz device batching
                // doesn't drop motion — every sample's delta contributes
                // to both the drag threshold and the scroll output.
                let mut primary_delta: Option<Point<Pixels>> = None;
                let pcount = event.pointers.len();
                for (i, p) in event.pointers.iter().enumerate() {
                    let Some(pstate) = self.pointers.get_mut(&p.id) else {
                        continue;
                    };
                    let mut prev = pstate.last_pos;
                    let mut total = Point::default();
                    for hpos in &p.history {
                        let step = *hpos - prev;
                        total = total + step;
                        pstate.accumulated_motion += step.magnitude();
                        prev = *hpos;
                    }
                    let step = p.pos - prev;
                    total = total + step;
                    pstate.accumulated_motion += step.magnitude();
                    pstate.last_pos = p.pos;
                    if i == 0 {
                        primary_delta = Some(total);
                    }
                }

                if pcount >= 2 {
                    // Multi-pointer classification. The scroll-target
                    // anchor is the primary's DOWN position (first
                    // finger's original landing). Same sticky rationale
                    // as single-finger scroll: GPUI hit-tests on
                    // `position`, and we want every Scroll event routed
                    // to whichever pane the user originally grabbed
                    // with finger 1, not whichever pane the centroid
                    // happens to drift over.
                    let primary_anchor = self
                        .pointers
                        .get(&event.pointers[0].id)
                        .map(|s| s.down_pos)
                        .unwrap_or(event.pointers[0].pos);
                    match self.phase {
                        GesturePhase::MultiFingerDown => {
                            // Ambiguous (2-finger-tap vs scroll). Commit
                            // to scroll once any pointer crosses the
                            // drag threshold. The 2-finger-tap path
                            // already fired (or didn't) at POINTER_DOWN
                            // time — if we're still in
                            // `MultiFingerDown`, the tap window expired
                            // or the second finger landed too far.
                            let crossed = self
                                .pointers
                                .values()
                                .any(|s| s.accumulated_motion >= DRAG_THRESHOLD_PX);
                            if !crossed {
                                return out;
                            }
                            self.phase = GesturePhase::MultiFingerScroll;
                            self.scroll_centroid = Some(centroid(&event.pointers));
                        }
                        GesturePhase::MultiFingerScroll => {
                            let cur = centroid(&event.pointers);
                            if let Some(prev) = self.scroll_centroid {
                                let delta = cur - prev;
                                if delta.magnitude() > 0.0 {
                                    out.push(PlatformInput::Scroll(ScrollEvent {
                                        position: primary_anchor,
                                        delta: ScrollDelta::Pixels(delta),
                                        modifiers,
                                        touch_phase: TouchPhase::Moved,
                                        source: ScrollSource::Touch,
                                    }));
                                }
                            }
                            self.scroll_centroid = Some(cur);
                        }
                        _ => {
                            // `RightClickResolved` (and any other phase
                            // that shouldn't see multi-pointer MOVE)
                            // just consumes the event; the gesture is
                            // already resolved.
                        }
                    }
                    return out;
                }

                // Single-pointer: classify tap vs scroll.
                let primary = &event.pointers[0];
                let primary_pos = primary.pos;
                let Some(pstate) = self.pointers.get(&primary.id) else {
                    return out;
                };
                let Some(delta) = primary_delta else {
                    return out;
                };

                match self.phase {
                    GesturePhase::SingleFingerDown if drag_capture => {
                        // An element claimed the gesture between our
                        // DOWN-dispatch and now (scrollbar thumb, etc).
                        // Transition to drag-passthrough: emit
                        // `MouseMove(Left held)` so the element tracks
                        // the finger. No scroll synthesis, no
                        // long-press transition for this gesture.
                        self.phase = GesturePhase::DragPassthrough;
                        out.push(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position: primary_pos,
                            pressed_button: Some(MouseButton::Left),
                            modifiers,
                        }));
                    }
                    GesturePhase::DragPassthrough => {
                        out.push(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position: primary_pos,
                            pressed_button: Some(MouseButton::Left),
                            modifiers,
                        }));
                    }
                    GesturePhase::SingleFingerDown
                        if pstate.accumulated_motion >= DRAG_THRESHOLD_PX =>
                    {
                        // Commit to scroll. Cancel the pending Left, then
                        // emit the first Scroll frame from the
                        // motion that pushed us over threshold.
                        //
                        // `position` stays anchored at the original DOWN
                        // location for the whole scroll gesture (NOT the
                        // current finger position) so GPUI's hit-test
                        // routes every Scroll event to whichever pane the
                        // user originally grabbed. Otherwise the scroll
                        // target switches mid-drag when the finger crosses
                        // a pane boundary (editor↔terminal split, etc.).
                        let anchor = pstate.down_pos;
                        self.phase = GesturePhase::SingleFingerScroll;
                        out.push(PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: anchor,
                            modifiers,
                            click_count: 0,
                        }));
                        if delta.magnitude() > 0.0 {
                            out.push(PlatformInput::Scroll(ScrollEvent {
                                position: anchor,
                                delta: ScrollDelta::Pixels(delta),
                                modifiers,
                                touch_phase: TouchPhase::Moved,
                                source: ScrollSource::Touch,
                            }));
                        }
                    }
                    GesturePhase::SingleFingerDown
                        if pstate.down_time.elapsed() >= LONG_PRESS_THRESHOLD =>
                    {
                        // Long-press fires. Motion stayed below
                        // `DRAG_THRESHOLD_PX` (the scroll arm above didn't
                        // match) AND we've been held past the long-press
                        // window. Cancel the pending `count=1` left-down
                        // and emit a `count=2` left-down at the original
                        // anchor — that's word-select in the editor.
                        // Selection extends via subsequent `MouseMove`s
                        // (handled in the `LongPressSelection` arm below).
                        // `MouseUp(count=2)` fires on the final lift in
                        // `TouchAction::Up`.
                        let anchor = pstate.down_pos;
                        self.phase = GesturePhase::LongPressSelection;
                        // Released where it went down, like a tap.
                        out.push(PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: anchor,
                            modifiers,
                            click_count: 0,
                        }));
                        out.push(PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: anchor,
                            modifiers,
                            click_count: 2,
                            first_mouse: false,
                        }));
                    }
                    GesturePhase::SingleFingerScroll => {
                        // Sticky anchor: see commit-to-scroll arm above.
                        let anchor = pstate.down_pos;
                        if delta.magnitude() > 0.0 {
                            out.push(PlatformInput::Scroll(ScrollEvent {
                                position: anchor,
                                delta: ScrollDelta::Pixels(delta),
                                modifiers,
                                touch_phase: TouchPhase::Moved,
                                source: ScrollSource::Touch,
                            }));
                        }
                    }
                    GesturePhase::LongPressSelection => {
                        // Finger is dragging post-long-press. Emit
                        // `MouseMove(Left held)` so the editor extends
                        // the selection to the current position.
                        out.push(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position: primary_pos,
                            pressed_button: Some(MouseButton::Left),
                            modifiers,
                        }));
                    }
                    _ => {}
                }
            }

            TouchAction::PointerUp { index } => {
                // A non-last finger lifted. Drop its state; the gesture
                // phase carries through (stays MultiFinger* until full UP).
                if index < event.pointers.len() {
                    let id = event.pointers[index].id;
                    self.pointers.remove(&id);
                }
            }

            TouchAction::Up => {
                // Final finger lifted. Resolve the gesture per the
                // phase we're closing out.
                let mut position = event.pointers[0].pos;
                // A tap releases where it went down, as gpui's own touch
                // recognizer does: with no move in between, the editor
                // takes a release elsewhere for a drag that never started.
                if matches!(self.phase, GesturePhase::SingleFingerDown)
                    && let Some(pointer) = self.pointers.get(&event.pointers[0].id)
                {
                    position = pointer.down_pos;
                }
                let close = match self.phase {
                    // Plain tap: emit `Up(Left, count=1)` to match the
                    // `Down(Left, count=1)` from `TouchAction::Down`.
                    GesturePhase::SingleFingerDown => Some(1usize),
                    // Long-press resolved: emit `Up(Left, count=2)` to
                    // match the `Down(Left, count=2)` from the long-
                    // press transition, locking the selection.
                    GesturePhase::LongPressSelection => Some(2usize),
                    // Drag-passthrough release: emit `Up(Left, count=0)`
                    // — it was a real drag, not a click. The dragging
                    // element's `MouseUp` handler runs (clearing the
                    // platform `drag_active` flag); `count=0` keeps the
                    // editor underneath from interpreting it as a
                    // click.
                    GesturePhase::DragPassthrough => Some(0usize),
                    // SingleFingerScroll, MultiFinger*, RightClickResolved:
                    // canceled or resolved at commit-time; nothing to emit.
                    _ => None,
                };
                self.reset();
                if let Some(click_count) = close {
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count,
                    }));
                }
            }

            TouchAction::Cancel => {
                let mut position = event
                    .pointers
                    .first()
                    .map(|p| p.pos)
                    .unwrap_or(Point::default());
                // A pending tap is released where it went down, like a tap.
                if matches!(self.phase, GesturePhase::SingleFingerDown)
                    && let Some(pointer) = event
                        .pointers
                        .first()
                        .and_then(|pointer| self.pointers.get(&pointer.id))
                {
                    position = pointer.down_pos;
                }
                // If we have an outstanding `Left` press (plain tap
                // pending, long-press selection, or drag-passthrough),
                // emit a `count=0` cancel `Up` so the editor doesn't
                // see a stuck button.
                let cancel = matches!(
                    self.phase,
                    GesturePhase::SingleFingerDown
                        | GesturePhase::LongPressSelection
                        | GesturePhase::DragPassthrough
                );
                self.reset();
                if cancel {
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count: 0,
                    }));
                }
            }
        }

        out
    }

    fn reset(&mut self) {
        self.pointers.clear();
        self.phase = GesturePhase::Idle;
        self.scroll_centroid = None;
    }
}

// ============================================================================
// Trackpad mode (VNC-style virtual trackpad)
// ============================================================================
//
// When `android_input.trackpad_mode` is enabled, the screen acts as a
// virtual trackpad rather than direct touch:
//
//   - 1-finger tap (no significant motion before lift) → click at the
//     virtual cursor's current position (`MouseDown` + `MouseUp`).
//   - 1-finger drag → advance the virtual cursor by the touch delta
//     and re-position the cursor sprite. No mouse button held.
//   - 2-finger tap → right-click at the cursor.
//   - 2-finger drag → `Scroll` based on centroid Y-delta.
//
// The virtual cursor lives on Kotlin's side (`MainActivity.cursorX/Y`,
// same SurfaceControl overlay the hardware trackpad uses) and we keep
// a Rust-side mirror in `TrackpadTouchState::cursor` so the emitted
// `MouseDown`/`MouseMove` events have the correct position. Each move
// fires a JNI call to `MainActivity.setTrackpadCursorPosition` to
// keep the sprite in sync.

/// Click vs drag threshold: motion above this on the touch surface
/// commits the gesture to drag (cursor motion); below it, lift is a
/// tap (click at cursor).
const TRACKPAD_TAP_THRESHOLD_PX: f64 = 8.0;

/// Minimum hold time before a one-finger touch promotes to
/// "hold-to-drag" (Left button held + cursor motion = text
/// selection / click-and-drag). Below this duration, finger
/// motion is interpreted as plain cursor motion (no button held);
/// at or above it, the gesture switches to drag-with-button so
/// the user can select text via the trackpad.
const TRACKPAD_HOLD_DRAG_THRESHOLD_MS: u128 = 300;

/// Per-axis acceleration applied to single-finger cursor-drag
/// deltas (no button held). Slow movement stays near 1:1 for
/// precision; faster movement gets a quadratic boost so the
/// cursor can cross the full screen without lifting and
/// re-touching. Capped to avoid runaway sensitivity on jitter.
///
/// `output = sign(delta) * |delta| * min(1 + |delta|² * 0.01, 4)`
///
/// Examples (one axis, logical pixels):
///   delta = 1px  → 1.01px (precision)
///   delta = 3px  → 3.27px
///   delta = 10px → 20px   (2x boost)
///   delta = 20px → 80px   (cap at 4x)
///
/// Not applied during hold-to-drag — text selection wants
/// precise 1:1 control.
fn accelerate_delta(delta_px: f32) -> f32 {
    let abs = delta_px.abs();
    let sign = delta_px.signum();
    let boost = (1.0 + abs * abs * 0.01).min(4.0);
    sign * abs * boost
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum TrackpadGesturePhase {
    #[default]
    Idle,
    /// One finger down, motion still below tap threshold AND
    /// elapsed time still below the hold-to-drag threshold. Lift
    /// → tap (click at cursor). Move past threshold →
    /// SingleFingerDrag. Hold past hold-drag threshold →
    /// HoldDrag.
    SingleFingerDown,
    /// One finger past tap threshold; subsequent moves advance the
    /// cursor (with acceleration), no mouse button held. Lift
    /// emits nothing — the gesture was pure navigation.
    SingleFingerDrag,
    /// One finger held still past the hold-to-drag threshold; we
    /// emitted MouseDown(Left) on the transition, subsequent moves
    /// emit MouseMove with the button held so the editor sees a
    /// drag (text selection on an editor, drag-and-drop elsewhere).
    /// 1:1 motion (no acceleration) for selection precision.
    /// Lift → MouseUp(count=1) to complete the click.
    HoldDrag,
    /// 2+ fingers down within the 2-finger-tap window. Not yet
    /// decided 2-finger-tap (right click) vs 2-finger-scroll.
    MultiFingerDown,
    /// 2+ fingers past scroll threshold; emits `Scroll`.
    MultiFingerScroll,
    /// 2-finger tap resolved as right-click. Waiting for all
    /// fingers to lift before returning to Idle.
    RightClickResolved,
}

#[derive(Debug, Clone)]
struct TrackpadPointerState {
    down_time: Instant,
    down_pos: Point<Pixels>,
    last_pos: Point<Pixels>,
    accumulated_motion: f64,
}

#[derive(Default)]
pub(crate) struct TrackpadTouchState {
    /// Per-pointer state for the active gesture.
    pointers: HashMap<i32, TrackpadPointerState>,
    phase: TrackpadGesturePhase,
    /// Last centroid for 2-finger scroll delta computation.
    scroll_centroid: Option<Point<Pixels>>,
    /// Virtual cursor position in physical pixels (decorView space —
    /// matches `MainActivity.cursorX/Y` convention). Updated by
    /// single-finger drag deltas; read by all gesture emit branches
    /// so synthesized mouse events fire at the right position.
    cursor: Point<Pixels>,
}

impl TrackpadTouchState {
    /// `android_app` is needed for the JNI call that moves the
    /// cursor sprite on the Kotlin side. `scale_factor` converts
    /// between gpui's logical-pixel coordinate space (`self.cursor`,
    /// emitted to MouseMove / MouseDown) and the physical-pixel
    /// space the Kotlin cursor sprite lives in (`MainActivity.cursorX/Y`,
    /// matching the captured-trackpad convention). Coordinates passed
    /// to gpui events come from `self.cursor`, not the raw touch
    /// position — that's the whole point of trackpad mode.
    pub(crate) fn on_event(
        &mut self,
        event: &TouchEvent,
        android_app: &AndroidApp,
        scale_factor: f32,
        extra_window_id: Option<u64>,
        bounds: gpui::Size<gpui::Pixels>,
    ) -> MotionInputs {
        let mut out = MotionInputs::new();
        if event.pointers.is_empty() {
            return out;
        }
        let modifiers = event.modifiers;

        match event.action {
            TouchAction::Down => {
                self.reset();
                let primary = &event.pointers[0];
                if self.cursor == Point::default() {
                    self.cursor = primary.pos;
                }
                self.pointers.insert(
                    primary.id,
                    TrackpadPointerState {
                        down_time: Instant::now(),
                        down_pos: primary.pos,
                        last_pos: primary.pos,
                        accumulated_motion: 0.0,
                    },
                );
                self.phase = TrackpadGesturePhase::SingleFingerDown;
                crate::cursor::move_trackpad_cursor(
                    android_app,
                    extra_window_id,
                    f32::from(self.cursor.x) * scale_factor,
                    f32::from(self.cursor.y) * scale_factor,
                );
                // Deliberately don't emit MouseDown here. Pre-emitting
                // would land as a "click outside" on any open
                // context menu / overlay whose dismiss handler
                // listens for MouseDown anywhere, and the user's
                // subsequent cursor-drag would dismiss it before
                // they could navigate to a menu item. Click emission
                // is deferred to TouchAction::Up (tap path) when we
                // know the gesture was a tap, not a drag.
            }

            TouchAction::PointerDown { index } => {
                let new_p = &event.pointers[index];
                self.pointers.insert(
                    new_p.id,
                    TrackpadPointerState {
                        down_time: Instant::now(),
                        down_pos: new_p.pos,
                        last_pos: new_p.pos,
                        accumulated_motion: 0.0,
                    },
                );
                if self.pointers.len() >= 2 {
                    self.phase = TrackpadGesturePhase::MultiFingerDown;
                    self.scroll_centroid = Some(centroid(&event.pointers));
                }
            }

            TouchAction::Move => match self.phase {
                TrackpadGesturePhase::SingleFingerDown
                | TrackpadGesturePhase::SingleFingerDrag
                | TrackpadGesturePhase::HoldDrag => {
                    let primary = &event.pointers[0];
                    let (delta, accumulated, elapsed_ms) = {
                        let Some(pstate) = self.pointers.get_mut(&primary.id) else {
                            return out;
                        };
                        let delta = primary.pos - pstate.last_pos;
                        pstate.last_pos = primary.pos;
                        pstate.accumulated_motion += delta.magnitude();
                        let elapsed = pstate.down_time.elapsed().as_millis();
                        (delta, pstate.accumulated_motion, elapsed)
                    };

                    // Phase transitions on the first Move that
                    // satisfies the respective threshold. Order
                    // matters: hold-to-drag wins over drag when
                    // both fire on the same event (user held still
                    // 300ms then jittered slightly — they want
                    // selection, not navigation).
                    if self.phase == TrackpadGesturePhase::SingleFingerDown {
                        if accumulated < TRACKPAD_TAP_THRESHOLD_PX
                            && elapsed_ms >= TRACKPAD_HOLD_DRAG_THRESHOLD_MS
                        {
                            // Hold-to-drag: emit MouseMove (hover
                            // refresh so the target gets pressed
                            // state on the next MouseDown) +
                            // MouseDown(Left). Subsequent moves
                            // emit MouseMove with the button held
                            // so the editor sees a held-button
                            // drag = text selection.
                            self.phase = TrackpadGesturePhase::HoldDrag;
                            out.push(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                position: self.cursor,
                                modifiers,
                                pressed_button: None,
                            }));
                            out.push(PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position: self.cursor,
                                modifiers,
                                click_count: 1,
                                first_mouse: false,
                            }));
                        } else if accumulated > TRACKPAD_TAP_THRESHOLD_PX {
                            // Plain drag (cursor navigation, no
                            // button held).
                            self.phase = TrackpadGesturePhase::SingleFingerDrag;
                        }
                    }

                    // Advance virtual cursor. Acceleration only on
                    // pure navigation; hold-to-drag stays 1:1 for
                    // text-selection precision.
                    let (scaled_dx, scaled_dy) =
                        if self.phase == TrackpadGesturePhase::SingleFingerDrag {
                            (
                                accelerate_delta(f32::from(delta.x)),
                                accelerate_delta(f32::from(delta.y)),
                            )
                        } else {
                            (f32::from(delta.x), f32::from(delta.y))
                        };
                    self.cursor.x += px(scaled_dx);
                    self.cursor.y += px(scaled_dy);
                    // Clamp to the window's visible bounds so we don't
                    // accumulate "ghost" travel past the edges. Without
                    // this, pushing the cursor past the right edge
                    // grows self.cursor.x unbounded; Kotlin's
                    // setTrackpadCursorPosition then clamps the DISPLAY
                    // to width-1 but Rust's internal state stays at the
                    // overshot value, so the user has to swipe back the
                    // exact same overshoot distance before the cursor
                    // visibly leaves the edge. The asymmetric look on
                    // the right edge (sprite extends past the screen)
                    // vs the left (sprite hugs the edge) is the Arrow
                    // hot-spot being in the top-left corner: when the
                    // hot-point sits at x=width-1 the sprite body
                    // extends sprite_size pixels further right, into
                    // off-screen territory, so the cursor "disappears
                    // outside the screen" until self.cursor.x drops
                    // back below width-1.
                    let zero = px(0.0);
                    if self.cursor.x < zero {
                        self.cursor.x = zero;
                    }
                    if self.cursor.y < zero {
                        self.cursor.y = zero;
                    }
                    if self.cursor.x > bounds.width {
                        self.cursor.x = bounds.width;
                    }
                    if self.cursor.y > bounds.height {
                        self.cursor.y = bounds.height;
                    }

                    let pressed = if self.phase == TrackpadGesturePhase::HoldDrag {
                        Some(MouseButton::Left)
                    } else {
                        None
                    };
                    out.push(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position: self.cursor,
                        modifiers,
                        pressed_button: pressed,
                    }));

                    crate::cursor::move_trackpad_cursor(
                        android_app,
                        extra_window_id,
                        f32::from(self.cursor.x) * scale_factor,
                        f32::from(self.cursor.y) * scale_factor,
                    );
                }
                TrackpadGesturePhase::MultiFingerDown | TrackpadGesturePhase::MultiFingerScroll => {
                    let new_centroid = centroid(&event.pointers);
                    let baseline = self.scroll_centroid.unwrap_or(new_centroid);
                    let delta = point(new_centroid.x - baseline.x, new_centroid.y - baseline.y);
                    self.scroll_centroid = Some(new_centroid);

                    if self.phase == TrackpadGesturePhase::MultiFingerDown
                        && delta.magnitude() > TRACKPAD_TAP_THRESHOLD_PX
                    {
                        // Promote to scroll once motion crosses threshold.
                        self.phase = TrackpadGesturePhase::MultiFingerScroll;
                    }

                    if self.phase == TrackpadGesturePhase::MultiFingerScroll {
                        out.push(PlatformInput::Scroll(ScrollEvent {
                            position: self.cursor,
                            delta: crate::ime::invert_scroll_delta(ScrollDelta::Pixels(delta)),
                            modifiers,
                            touch_phase: TouchPhase::Moved,
                            source: ScrollSource::Pointer,
                        }));
                    }
                }
                _ => {}
            },

            TouchAction::PointerUp { index } => {
                let p = &event.pointers[index];
                self.pointers.remove(&p.id);

                // 2-finger tap (no motion past threshold) → right click.
                if matches!(self.phase, TrackpadGesturePhase::MultiFingerDown) {
                    out.push(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Right,
                        position: self.cursor,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }));
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Right,
                        position: self.cursor,
                        modifiers,
                        click_count: 1,
                    }));
                    self.phase = TrackpadGesturePhase::RightClickResolved;
                }
            }

            TouchAction::Up => {
                let phase = self.phase;
                self.reset();
                match phase {
                    TrackpadGesturePhase::SingleFingerDown => {
                        // Quick tap without significant motion =
                        // click at cursor. Emit MouseMove (hover
                        // refresh, so hover-only elements like tab
                        // close are armed) + MouseDown + MouseUp.
                        // All three land in the same event batch
                        // but gpui's click handler completes the
                        // click on the matching MouseUp regardless.
                        out.push(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position: self.cursor,
                            modifiers,
                            pressed_button: None,
                        }));
                        out.push(PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: self.cursor,
                            modifiers,
                            click_count: 1,
                            first_mouse: false,
                        }));
                        out.push(PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: self.cursor,
                            modifiers,
                            click_count: 1,
                        }));
                    }
                    TrackpadGesturePhase::HoldDrag => {
                        // Hold-to-drag ended. MouseDown(Left) was
                        // emitted on the hold transition;
                        // MouseMoves with the button held fired
                        // during the drag. Close the gesture with
                        // MouseUp(count=1) at the final cursor
                        // position so the editor completes its
                        // selection / drag operation.
                        out.push(PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: self.cursor,
                            modifiers,
                            click_count: 1,
                        }));
                    }
                    _ => {
                        // SingleFingerDrag / MultiFingerScroll /
                        // RightClickResolved: no MouseDown was ever
                        // emitted; cursor motion / scroll / right-
                        // click already fired their events.
                    }
                }
            }

            TouchAction::Cancel => {
                let phase = self.phase;
                self.reset();
                if matches!(phase, TrackpadGesturePhase::HoldDrag) {
                    // OS canceled the gesture mid-hold-drag. Close
                    // the pending click with MouseUp(count=0) so
                    // the editor doesn't see a stuck Left button
                    // and doesn't commit a selection on a canceled
                    // gesture.
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: self.cursor,
                        modifiers,
                        click_count: 0,
                    }));
                }
            }
        }
        out
    }

    fn reset(&mut self) {
        self.pointers.clear();
        self.phase = TrackpadGesturePhase::Idle;
        self.scroll_centroid = None;
    }
}

// ============================================================================
// Raw touches for gpui's gesture recognizer
// ============================================================================
//
// With `android_input.legacy_touch` off, `TouchFeed` passes each finger to
// gpui as raw `PlatformInput::Touch` events, and gpui's recognizer
// (`gpui::gestures`) turns them into taps, pans with momentum and long
// presses. That recognizer follows one touch at a time, so `TouchFeed`
// resolves two-finger gestures itself: a two-finger tap becomes a secondary
// click, and a two-finger drag a single touch at the fingers' centroid.

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

    /// Cancels every touch gpui follows, for when the rest of a gesture
    /// goes elsewhere: gpui's recognizer takes no new touch while one is
    /// open.
    pub(crate) fn abandon(&mut self) -> MotionInputs {
        let mut out = MotionInputs::new();
        self.cancel_all(&mut out);
        out
    }

    /// `now` is when the event arrived. Batched history samples aren't
    /// replayed: gpui timestamps touches as they arrive, so they would look
    /// simultaneous and distort the fling speed.
    pub(crate) fn on_event(&mut self, event: &TouchEvent, now: Instant) -> MotionInputs {
        let mut out = MotionInputs::new();
        match event.action {
            TouchAction::Down => {
                self.cancel_all(&mut out);
                if let Some(pointer) = event.pointers.first()
                    && !pointer.palm
                {
                    self.start(pointer, &mut out);
                }
            }
            TouchAction::PointerDown { index } => {
                self.reject_palms(event, &mut out);
                if let Some(pointer) = event.pointers.get(index)
                    && !pointer.palm
                {
                    self.pointer_down(pointer, now, &mut out);
                }
            }
            TouchAction::Move => {
                self.reject_palms(event, &mut out);
                self.pointers_moved(event, &mut out);
            }
            TouchAction::PointerUp { index } => {
                self.reject_palms(event, &mut out);
                if let Some(pointer) = event.pointers.get(index) {
                    self.pointer_up(pointer, event, now, &mut out);
                }
            }
            TouchAction::Up => {
                self.reject_palms(event, &mut out);
                if let Some(pointer) = event.pointers.first() {
                    self.pointer_up(pointer, event, now, &mut out);
                }
                self.cancel_all(&mut out);
            }
            TouchAction::Cancel => self.cancel_all(&mut out),
        }
        out
    }

    fn start(&mut self, pointer: &TouchPointer, out: &mut MotionInputs) {
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
                start: pointer.pos,
                position: pointer.pos,
                left_slop: false,
            },
        );
        out.push(touch_input(id, TouchPhase::Started, pointer.pos));
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

    fn pointer_down(&mut self, pointer: &TouchPointer, now: Instant, out: &mut MotionInputs) {
        match self.two_finger {
            TwoFingerGesture::None => {
                if let Some(first_pointer_id) = self.sole_touch_within_slop()
                    && let Some(first) = self.open_touches.remove(&first_pointer_id)
                {
                    out.push(touch_input(first.id, TouchPhase::Cancelled, first.position));
                    self.two_finger = TwoFingerGesture::Pending {
                        anchor: first.start,
                        second_landed_at: now,
                        fingers: [(first_pointer_id, first.start), (pointer.id, pointer.pos)],
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

    fn pointers_moved(&mut self, event: &TouchEvent, out: &mut MotionInputs) {
        let touch_slop = f64::from(self.touch_slop);
        for pointer in &event.pointers {
            let Some(touch) = self.open_touches.get_mut(&pointer.id) else {
                continue;
            };
            if touch.position == pointer.pos {
                continue;
            }
            touch.position = pointer.pos;
            touch.left_slop |= (pointer.pos - touch.start).magnitude() > touch_slop;
            out.push(touch_input(touch.id, TouchPhase::Moved, pointer.pos));
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
                            && (pointer.pos - *landing_point).magnitude() > tap_slop
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
        event: &TouchEvent,
        fingers: [i32; 2],
        out: &mut MotionInputs,
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
        pointer: &TouchPointer,
        event: &TouchEvent,
        now: Instant,
        out: &mut MotionInputs,
    ) {
        if let Some(touch) = self.open_touches.remove(&pointer.id) {
            let phase = if event.canceled {
                TouchPhase::Cancelled
            } else {
                TouchPhase::Ended
            };
            out.push(touch_input(touch.id, phase, pointer.pos));
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
                    out.push(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Right,
                        position: anchor,
                        modifiers: event.modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }));
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Right,
                        position: anchor,
                        modifiers: event.modifiers,
                        click_count: 1,
                    }));
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
    fn reject_palms(&mut self, event: &TouchEvent, out: &mut MotionInputs) {
        for pointer in event.pointers.iter().filter(|pointer| pointer.palm) {
            if let Some(touch) = self.open_touches.remove(&pointer.id) {
                out.push(touch_input(touch.id, TouchPhase::Cancelled, pointer.pos));
            }
            if self.two_finger.follows(pointer.id) {
                self.ignore_two_finger_gesture(out);
            }
        }
    }

    fn ignore_two_finger_gesture(&mut self, out: &mut MotionInputs) {
        if let TwoFingerGesture::Panning { touch, .. } =
            mem::replace(&mut self.two_finger, TwoFingerGesture::Ignored)
        {
            out.push(touch_input(touch.id, TouchPhase::Cancelled, touch.position));
        }
    }

    fn cancel_all(&mut self, out: &mut MotionInputs) {
        for (_, touch) in self.open_touches.drain() {
            out.push(touch_input(touch.id, TouchPhase::Cancelled, touch.position));
        }
        if let TwoFingerGesture::Panning { touch, .. } = mem::take(&mut self.two_finger) {
            out.push(touch_input(touch.id, TouchPhase::Cancelled, touch.position));
        }
    }
}

/// The centroid of the given pointers, if the event has all of them.
fn centroid_of(pointers: &[TouchPointer], pointer_ids: &[i32]) -> Option<Point<Pixels>> {
    let mut sum = Point::<Pixels>::default();
    for pointer_id in pointer_ids {
        let pointer = pointers.iter().find(|pointer| pointer.id == *pointer_id)?;
        sum = sum + pointer.pos;
    }
    let count = pointer_ids.len() as f32;
    (count > 0.0).then(|| point(sum.x / count, sum.y / count))
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
    use super::*;

    #[derive(Debug, PartialEq)]
    enum Output {
        Touch(u64, TouchPhase, (f32, f32)),
        SecondaryDown((f32, f32)),
        SecondaryUp((f32, f32)),
    }

    fn outputs(inputs: MotionInputs) -> Vec<Output> {
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

    fn finger(id: i32, x: f32, y: f32) -> TouchPointer {
        TouchPointer {
            id,
            pos: point(px(x), px(y)),
            history: Vec::new(),
            palm: false,
        }
    }

    fn palm(id: i32, x: f32, y: f32) -> TouchPointer {
        TouchPointer {
            palm: true,
            ..finger(id, x, y)
        }
    }

    fn event(action: TouchAction, pointers: Vec<TouchPointer>) -> TouchEvent {
        TouchEvent {
            action,
            modifiers: gpui::Modifiers::default(),
            pointers,
            canceled: false,
        }
    }

    fn feed() -> TouchFeed {
        TouchFeed::new(px(8.0))
    }

    #[test]
    fn touch_ids_are_never_reused() {
        let mut feed = feed();
        let now = Instant::now();
        let down = event(TouchAction::Down, vec![finger(0, 10.0, 10.0)]);
        let up = event(TouchAction::Up, vec![finger(0, 10.0, 10.0)]);
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
        feed.on_event(&event(TouchAction::Down, vec![finger(0, 10.0, 10.0)]), now);
        assert_eq!(
            outputs(feed.on_event(&event(TouchAction::Down, vec![finger(0, 30.0, 30.0)]), now)),
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
        feed.on_event(&event(TouchAction::Down, vec![finger(0, 10.0, 10.0)]), now);
        let mut up = event(TouchAction::Up, vec![finger(0, 10.0, 10.0)]);
        up.canceled = true;
        assert_eq!(
            outputs(feed.on_event(&up, now)),
            [Output::Touch(0, TouchPhase::Cancelled, (10.0, 10.0))]
        );

        feed.on_event(&event(TouchAction::Down, vec![finger(0, 10.0, 10.0)]), now);
        let moved = event(TouchAction::Move, vec![palm(0, 12.0, 10.0)]);
        assert_eq!(
            outputs(feed.on_event(&moved, now)),
            [Output::Touch(1, TouchPhase::Cancelled, (12.0, 10.0))]
        );
        let up = event(TouchAction::Up, vec![palm(0, 12.0, 10.0)]);
        assert!(outputs(feed.on_event(&up, now)).is_empty());
    }

    #[test]
    fn two_finger_tap_clicks_secondary_on_lift() {
        let mut feed = feed();
        let start = Instant::now();
        let second_landed = start + Duration::from_millis(50);
        feed.on_event(
            &event(TouchAction::Down, vec![finger(0, 100.0, 100.0)]),
            start,
        );
        let second_down = event(
            TouchAction::PointerDown { index: 1 },
            vec![finger(0, 101.0, 100.0), finger(1, 200.0, 100.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&second_down, second_landed)),
            [Output::Touch(0, TouchPhase::Cancelled, (100.0, 100.0))]
        );
        let jitter = event(
            TouchAction::Move,
            vec![finger(0, 104.0, 100.0), finger(1, 203.0, 100.0)],
        );
        assert!(outputs(feed.on_event(&jitter, second_landed)).is_empty());
        let first_up = event(
            TouchAction::PointerUp { index: 1 },
            vec![finger(0, 104.0, 100.0), finger(1, 203.0, 100.0)],
        );
        assert!(
            outputs(feed.on_event(&first_up, second_landed + Duration::from_millis(150)))
                .is_empty()
        );
        let last_up = event(TouchAction::Up, vec![finger(0, 104.0, 100.0)]);
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
            &event(TouchAction::Down, vec![finger(0, 100.0, 100.0)]),
            start,
        );
        feed.on_event(
            &event(
                TouchAction::PointerDown { index: 1 },
                vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
            ),
            start,
        );
        let late = start + TWO_FINGER_TAP_TIMEOUT + Duration::from_millis(1);
        let first_up = event(
            TouchAction::PointerUp { index: 1 },
            vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
        );
        assert!(outputs(feed.on_event(&first_up, late)).is_empty());
        let last_up = event(TouchAction::Up, vec![finger(0, 100.0, 100.0)]);
        assert!(outputs(feed.on_event(&last_up, late)).is_empty());
    }

    #[test]
    fn two_finger_drag_pans_at_the_centroid() {
        let mut feed = feed();
        let now = Instant::now();
        feed.on_event(
            &event(TouchAction::Down, vec![finger(0, 100.0, 100.0)]),
            now,
        );
        feed.on_event(
            &event(
                TouchAction::PointerDown { index: 1 },
                vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
            ),
            now,
        );
        let drag = event(
            TouchAction::Move,
            vec![finger(0, 100.0, 130.0), finger(1, 200.0, 130.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&drag, now)),
            [Output::Touch(1, TouchPhase::Started, (150.0, 130.0))]
        );
        let drag = event(
            TouchAction::Move,
            vec![finger(0, 100.0, 140.0), finger(1, 200.0, 140.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&drag, now)),
            [Output::Touch(1, TouchPhase::Moved, (150.0, 140.0))]
        );
        let first_up = event(
            TouchAction::PointerUp { index: 0 },
            vec![finger(0, 100.0, 140.0), finger(1, 200.0, 140.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&first_up, now)),
            [Output::Touch(1, TouchPhase::Ended, (150.0, 140.0))]
        );
        let drag = event(TouchAction::Move, vec![finger(1, 200.0, 160.0)]);
        assert!(outputs(feed.on_event(&drag, now)).is_empty());
        let last_up = event(TouchAction::Up, vec![finger(1, 200.0, 160.0)]);
        assert!(outputs(feed.on_event(&last_up, now)).is_empty());
    }

    #[test]
    fn third_finger_cancels_the_pan() {
        let mut feed = feed();
        let now = Instant::now();
        feed.on_event(
            &event(TouchAction::Down, vec![finger(0, 100.0, 100.0)]),
            now,
        );
        feed.on_event(
            &event(
                TouchAction::PointerDown { index: 1 },
                vec![finger(0, 100.0, 100.0), finger(1, 200.0, 100.0)],
            ),
            now,
        );
        feed.on_event(
            &event(
                TouchAction::Move,
                vec![finger(0, 100.0, 130.0), finger(1, 200.0, 130.0)],
            ),
            now,
        );
        let third_down = event(
            TouchAction::PointerDown { index: 2 },
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
            TouchAction::Move,
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
            &event(TouchAction::Down, vec![finger(0, 100.0, 100.0)]),
            now,
        );
        let drag = event(TouchAction::Move, vec![finger(0, 100.0, 120.0)]);
        assert_eq!(
            outputs(feed.on_event(&drag, now)),
            [Output::Touch(0, TouchPhase::Moved, (100.0, 120.0))]
        );
        let second_down = event(
            TouchAction::PointerDown { index: 1 },
            vec![finger(0, 100.0, 120.0), finger(1, 200.0, 100.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&second_down, now)),
            [Output::Touch(1, TouchPhase::Started, (200.0, 100.0))]
        );
        let first_up = event(
            TouchAction::PointerUp { index: 0 },
            vec![finger(0, 100.0, 120.0), finger(1, 200.0, 100.0)],
        );
        assert_eq!(
            outputs(feed.on_event(&first_up, now)),
            [Output::Touch(0, TouchPhase::Ended, (100.0, 120.0))]
        );
        let last_up = event(TouchAction::Up, vec![finger(1, 200.0, 100.0)]);
        assert_eq!(
            outputs(feed.on_event(&last_up, now)),
            [Output::Touch(1, TouchPhase::Ended, (200.0, 100.0))]
        );
    }
}
