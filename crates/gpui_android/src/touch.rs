//! Finger input for gpui.
//!
//! Touch on Android (`SOURCE_TOUCHSCREEN` + `ToolType::Finger`) arrives
//! through android-activity's NDK native input queue for the main window
//! and as JNI-marshaled `MotionEvent` fields for extra windows. Both are
//! normalized into a [`TouchEvent`] here. Then [`TouchFeed`] passes the
//! fingers to gpui's own gesture recognizer (`gpui::gestures`) as raw
//! `PlatformInput::Touch` events, or, with `android_input.trackpad_mode`
//! on, [`TrackpadTouchState`] turns them into mouse events at a virtual
//! cursor.
//!
//! Peer of `captured_pointer.rs` (captured-trackpad SM). In the main
//! window, mouse, stylus and the captured-trackpad path flow through their
//! own dispatchers in `events.rs` and only finger input routes here. Extra
//! windows send every touch action here, whatever the tool, since
//! `ExtraWindowActivity.forwardTouchEvent` doesn't report the source.
//!
//! State ownership: each `AndroidWindowState` owns its own `TouchFeed` and
//! `TrackpadTouchState`, so gestures in different windows can't interfere.

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
    JAVA_ACTION_POINTER_UP, JAVA_ACTION_UP, JAVA_FLAG_CANCELED, JAVA_TOOL_TYPE_PALM,
};
use crate::events::{MotionInputs, TranslatedMotion};
use crate::window::AndroidWindowState;

/// Normalized touch input. Built from either an Android `MotionEvent`
/// (primary surface) or the JNI-marshaled fields (extras). All
/// coordinates are *logical pixels*; the source-specific builders divide
/// by `scale_factor`.
pub(crate) struct TouchEvent {
    pub action: TouchAction,
    pub modifiers: gpui::Modifiers,
    pub pointers: Vec<TouchPointer>,
    /// `MotionEvent.FLAG_CANCELED`: the pointer going up was cancelled.
    pub canceled: bool,
}

/// Java `MotionEvent.ACTION_*` collapsed to the touch-relevant subset; the
/// non-touch actions (HOVER_*, SCROLL, BUTTON_*) never reach this module
/// and aren't represented.
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
/// Coordinates are logical pixels.
pub(crate) struct TouchPointer {
    pub id: i32,
    pub pos: Point<Pixels>,
    /// `MotionEvent.TOOL_TYPE_PALM`: the system decided this contact is a
    /// palm resting on the screen.
    pub palm: bool,
}

pub(crate) fn dispatch_primary(
    state: &mut AndroidWindowState,
    event: &MotionEvent,
    scale_factor: f32,
) -> TranslatedMotion {
    let Some(touch_event) = build_from_motion_event(event, scale_factor) else {
        return TranslatedMotion::default();
    };
    dispatch(state, &touch_event, scale_factor)
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
    dispatch(state, &touch_event, scale_factor).inputs
}

/// Sends a touch event to the virtual trackpad or to gpui's gesture
/// recognizer.
fn dispatch(
    state: &mut AndroidWindowState,
    touch_event: &TouchEvent,
    scale_factor: f32,
) -> TranslatedMotion {
    if crate::ime::trackpad_mode_enabled() {
        let mut inputs = state.touch_feed.abandon();
        let android_app = state.android_app.clone();
        let extra_window_id = state.extra_window_id;
        // The cursor stops above the on-screen keyboard and the extras row:
        // they would hide it, and there's nothing of gpui's below them.
        let bounds = state.layout_bounds().size;
        inputs.extend(state.trackpad_touch.on_event(
            touch_event,
            &android_app,
            scale_factor,
            extra_window_id,
            bounds,
        ));
        return inputs.into();
    }
    TranslatedMotion {
        inputs: state.touch_feed.on_event(touch_event, Instant::now()),
        handled: true,
    }
}

/// Build a [`TouchEvent`] from an Android NDK `MotionEvent`.
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
        pointers.push(TouchPointer {
            id: p.pointer_id(),
            pos,
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
/// pipeline sends.
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
// `TouchFeed` passes each finger to gpui as raw `PlatformInput::Touch`
// events, and gpui's recognizer (`gpui::gestures`) turns them into taps,
// pans with momentum and long presses. That recognizer follows one touch at a time, so `TouchFeed`
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
