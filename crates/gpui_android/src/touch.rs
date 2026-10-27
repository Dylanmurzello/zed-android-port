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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use android_activity::input::{MetaState, MotionAction, MotionEvent};
use gpui::{
    MouseButton, MouseDownEvent, MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta,
    ScrollEvent, ScrollSource, TouchPhase, point, px,
};

use crate::events::MotionInputs;
use crate::events::keyboard;
use crate::events::{
    JAVA_ACTION_CANCEL, JAVA_ACTION_DOWN, JAVA_ACTION_MOVE, JAVA_ACTION_POINTER_DOWN,
    JAVA_ACTION_POINTER_UP, JAVA_ACTION_UP,
};
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
}

pub(crate) fn dispatch_primary(
    state: &mut AndroidWindowState,
    event: &MotionEvent,
    scale_factor: f32,
) -> MotionInputs {
    let Some(touch_event) = build_from_motion_event(event, scale_factor) else {
        return MotionInputs::new();
    };
    if crate::ime::trackpad_mode_enabled() {
        let android_app = state.android_app.clone();
        let extra_window_id = state.extra_window_id;
        let bounds = state.bounds.size;
        return state.trackpad_touch.on_event(
            &touch_event,
            &android_app,
            scale_factor,
            extra_window_id,
            bounds,
        );
    }
    let drag_capture = state.drag_active.load(std::sync::atomic::Ordering::Relaxed);
    state.touch.on_event(&touch_event, drag_capture)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_extra(
    state: &mut AndroidWindowState,
    _window_id: u64,
    action_masked: i32,
    action_index: i32,
    meta_state: i32,
    _button_state: i32,
    _vscroll: f32,
    _hscroll: f32,
    positions: &[(f32, f32, i32)],
    scale_factor: f32,
) -> MotionInputs {
    let Some(touch_event) = build_from_extra_fields(
        action_masked,
        action_index,
        meta_state,
        positions,
        scale_factor,
    ) else {
        return MotionInputs::new();
    };
    if crate::ime::trackpad_mode_enabled() {
        let android_app = state.android_app.clone();
        let extra_window_id = state.extra_window_id;
        let bounds = state.bounds.size;
        return state.trackpad_touch.on_event(
            &touch_event,
            &android_app,
            scale_factor,
            extra_window_id,
            bounds,
        );
    }
    let drag_capture = state.drag_active.load(std::sync::atomic::Ordering::Relaxed);
    state.touch.on_event(&touch_event, drag_capture)
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
        });
    }
    Some(TouchEvent {
        action,
        modifiers,
        pointers,
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
    positions: &[(f32, f32, i32)],
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
        .map(|(x, y, id)| TouchPointer {
            id: *id,
            pos: point(px(x / scale_factor), px(y / scale_factor)),
            history: Vec::new(),
        })
        .collect();
    Some(TouchEvent {
        action,
        modifiers,
        pointers,
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
