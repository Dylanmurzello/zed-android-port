//! Pointer-capture event synthesis.
//!
//! When `MainActivity.kt` calls `decorView.requestPointerCapture()` and
//! it succeeds, the trackpad delivers raw `MotionEvent`s to our
//! `OnCapturedPointerListener` instead of being filtered through
//! Android's gesture-detector layer. Captured events arrive tagged
//! `SOURCE_TOUCHPAD` (not `SOURCE_MOUSE`) with full multi-touch state,
//! per-pointer `AXIS_RELATIVE_X/Y` deltas, and clean
//! `ACTION_BUTTON_PRESS` / `ACTION_BUTTON_RELEASE` framing.
//!
//! This module turns that raw stream into the gpui `PlatformInput`
//! vocabulary so the editor sees normal `MouseMove` / `MouseDown` /
//! `MouseUp` / `ScrollWheel` events. We keep our own cursor position
//! (the system cursor is hidden while capture is active) and update it
//! by accumulating per-event relative deltas. Single-finger motion
//! moves the cursor; two-finger motion synthesizes a scroll from the
//! centroid delta and pins the cursor in place.
//!
//! ## Event flow
//!
//! Kotlin builds a `CapturedEvent` from each captured `MotionEvent` and
//! calls `nativeOnCapturedPointer` (JNI). The Rust handler pushes the
//! event onto a static unbounded channel; the platform's main loop
//! drains the channel each iteration and feeds the synthesized
//! `PlatformInput`s into the primary window's `handle_input`. Cross-
//! thread because Kotlin's listener fires on the UI thread but
//! `handle_input` must run on the game thread.
//!
//! ## Gestures (matching desktop trackpad standards)
//!
//! Single-finger:
//! - Plain tap (DOWN → UP within `TAP_WINDOW`, motion < `TAP_MOTION_PX`):
//!   synthesize `MouseDown(Left)` + `MouseUp(Left)` at cursor — left
//!   click.
//! - Tap-tap-drag (per libinput/Windows Precision Touchpad standard):
//!   plain tap, then a second `ACTION_DOWN` within `TAP_DRAG_WINDOW`
//!   of the first tap-up near the same position. On the first MOVE
//!   past `TAP_DRAG_SLOP`, emit `MouseDown(Left)` at the tap anchor
//!   and treat subsequent motion as click-and-drag. ACTION_UP ends
//!   the drag. This is how text selection / rubber-band / drag-and-
//!   drop is invoked on every major desktop trackpad spec.
//! - `ACTION_BUTTON_PRESS` (Samsung's driver-recognized drag): emit
//!   `MouseDown(Left)`, follow with MouseMove(Left held), release on
//!   `ACTION_BUTTON_RELEASE`. Coexists with tap-tap-drag for
//!   trackpads that don't emit BUTTON_PRESS reliably in capture mode.
//!
//! Two-finger:
//! - Quick tap-tap (both fingers down + up within `TWO_FINGER_TAP_WINDOW`
//!   with low motion): right-click at cursor.
//! - Hold-and-drag: finger 1 is on the pad for at least
//!   `HOLD_DRAG_THRESHOLD` before finger 2 lands. Synthesize
//!   `MouseDown(Left)` at finger 1's anchor on `ACTION_POINTER_DOWN`,
//!   accumulate finger 2's relative deltas into an internal drag
//!   cursor, emit `MouseMove(Left held)` at the drag cursor on each
//!   MOVE, and release on either pointer lifting. This is how
//!   Samsung's stock trackpad driver recognizes click-and-drag, and
//!   matches what users observe on Android trackpads when selecting
//!   text. Discriminates from scroll because scroll lands both fingers
//!   near-simultaneously (hold-elapsed < threshold).
//! - Move (both fingers landed near-simultaneously): scroll. The
//!   centroid delta drives `ScrollDelta::Pixels`. Sign matches
//!   `events/trackpad.rs` so behavior is consistent with the
//!   non-captured trackpad path.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use gpui::{
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection,
    Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px,
};
use jni::objects::{JFloatArray, JObject, JString};

/// Java `MotionEvent.getActionMasked()` constants. Mirrors the ones in
/// `events.rs` for the same JNI reason: `MotionAction` from the NDK
/// crate has a private constructor.
const JAVA_ACTION_DOWN: i32 = 0;
const JAVA_ACTION_UP: i32 = 1;
const JAVA_ACTION_MOVE: i32 = 2;
const JAVA_ACTION_CANCEL: i32 = 3;
const JAVA_ACTION_POINTER_DOWN: i32 = 5;
const JAVA_ACTION_POINTER_UP: i32 = 6;
const JAVA_ACTION_BUTTON_PRESS: i32 = 11;
const JAVA_ACTION_BUTTON_RELEASE: i32 = 12;

/// Android `MotionEvent.BUTTON_*` constants. Mirrors `events/mouse.rs`.
const ANDROID_BUTTON_PRIMARY: i32 = 1 << 0;
const ANDROID_BUTTON_SECONDARY: i32 = 1 << 1;
const ANDROID_BUTTON_TERTIARY: i32 = 1 << 2;
const ANDROID_BUTTON_BACK: i32 = 1 << 3;
const ANDROID_BUTTON_FORWARD: i32 = 1 << 4;

/// `InputDevice.SOURCE_TOUCHPAD` = SOURCE_CLASS_POSITION (0x8) plus
/// the touchpad bit (0x100000). Used to source-filter BUTTON_PRESS /
/// BUTTON_RELEASE: verified empirically (logs show source=0x100008)
/// that the Book Cover trackpad firmware fires these on every finger
/// contact, not just real clicks. For a real mouse plugged into the
/// tablet (SOURCE_MOUSE = 0x2002), BUTTON_PRESS is a genuine click
/// and must still be honored.
const ANDROID_SOURCE_TOUCHPAD: i32 = 0x00100008;

/// Tap detection: a single-finger `ACTION_DOWN` → `ACTION_UP` within
/// this time, with less than `TAP_MOTION_PX` of accumulated motion,
/// counts as a click and synthesizes `MouseDown` + `MouseUp(Left)`.
const TAP_WINDOW: Duration = Duration::from_millis(200);
const TAP_MOTION_PX: f32 = 6.0;

/// Two-finger tap → right-click detection. The second finger must
/// land within this window of the first and the gesture must not
/// have drifted past `TWO_FINGER_TAP_MOTION_PX` yet. Arms the right-
/// click on `ACTION_POINTER_DOWN`; the click only fires on
/// `ACTION_POINTER_UP` so a two-finger SCROLL (which moves before
/// lifting) disarms via the motion-accum path and doesn't accidentally
/// pop a context menu mid-scroll. Matches `events/touch.rs`'s
/// touchscreen two-finger-tap behavior for consistency.
const TWO_FINGER_TAP_WINDOW: Duration = Duration::from_millis(300);
const TWO_FINGER_TAP_MOTION_PX: f32 = 6.0;

/// Tap-tap-drag window: time from the first tap's UP to the second
/// tap's DOWN. Per libinput's tap state machine, 300ms is the
/// implementation-defined timeout. If the second touch lands within
/// this window AND near the first tap's position, it qualifies as the
/// start of a tap-tap-drag rather than an unrelated second tap.
const TAP_DRAG_WINDOW: Duration = Duration::from_millis(300);

/// "Hold finger 1, drag with finger 2" drag-lock threshold. When the
/// second finger lands and the first finger has already been on the
/// pad for at least this long, interpret the gesture as a deliberate
/// hold-and-drag (Samsung's trackpad recognizes this as
/// click-and-drag; users widely report this gesture is what they
/// expect for text selection on Android trackpads). Below this
/// threshold (both fingers land near-simultaneously) the gesture is
/// scroll territory. 100ms is short enough that intentional holds
/// always register while preventing accidental engagement on
/// fast scroll gestures.
const HOLD_DRAG_THRESHOLD: Duration = Duration::from_millis(100);

/// Stationary requirement for the hold-finger-drag-finger gesture.
/// Finger 1 must have been still (no `AXIS_RELATIVE_*` motion) for at
/// least this long before finger 2 lands; otherwise the user is
/// actively driving the cursor with finger 1, and the second finger
/// landing means "scroll" not "start selection." Without this guard,
/// any time the user has finger 1 on the pad (even just resting it
/// while reading) and places finger 2 to scroll, hold-drag erroneously
/// engages and starts selecting text on the scroll gesture.
const HOLD_DRAG_STATIONARY: Duration = Duration::from_millis(80);

/// Tap-tap-drag position slop: the second touch must land within
/// this many logical pixels of the first tap's position to count as
/// part of the same gesture. Matches the click-position slop used by
/// our existing multi-click detection so feel is consistent.
const TAP_DRAG_POSITION_SLOP_PX: f32 = 6.0;

/// Tap-tap-drag motion-to-engage: how far the finger must move after
/// the second-tap DOWN before we commit to drag mode (and emit
/// `MouseDown(Left)`). Below this the system can still resolve the
/// gesture as a regular double-tap if the user lifts the finger
/// without dragging.
const TAP_DRAG_SLOP_PX: f32 = 2.0;

/// libinput "drag lock" timeout. After a tap-tap-drag's ACTION_UP we
/// hold the button down for this long instead of releasing
/// immediately. Within this window a fresh ACTION_DOWN continues the
/// drag (useful when the trackpad is too small to reach the
/// selection's end in one swipe; user lifts, repositions, and
/// continues). After the timeout the next event auto-ends the drag.
/// 500ms matches libinput's documented default.
const DRAG_LOCK_TIMEOUT: Duration = Duration::from_millis(500);

/// Tap-to-end window during drag lock: a fresh ACTION_DOWN within
/// the lock that ends in ACTION_UP within this time, with motion
/// under `TAP_MOTION_PX`, ends the drag (emits MouseUp). A longer
/// hold or any meaningful motion continues the drag instead.
const DRAG_LOCK_TAP_WINDOW: Duration = Duration::from_millis(200);


/// `window_id` sentinel for the primary `MainActivity` window. Extra
/// windows use their gpui-assigned non-zero `WindowId` so per-window
/// state never collides with the primary.
pub(crate) const PRIMARY_WINDOW_ID: u64 = 0;

/// Captured `MotionEvent` shape marshaled across JNI. One per event.
/// Pointer fields are indexed 0..pointer_count. `cursor_physical_*`
/// is Kotlin's authoritative cursor position in physical pixels
/// (decorView coordinate space) — Kotlin maintains it because the
/// software cursor View has to be positioned in the same space.
/// `window_id` is `PRIMARY_WINDOW_ID` for MainActivity events and
/// the extra window's id (matching `multi_window::ExtraWindowEvent`)
/// for spawned windows. State machines + hold-drag atomics are
/// per-window so a gesture in window A can't pollute window B's
/// drag-lock or cursor-follow state.
#[derive(Debug)]
pub(crate) struct CapturedEvent {
    pub window_id: u64,
    pub action_masked: i32,
    pub source: i32,
    pub button_state: i32,
    pub pointer_count: usize,
    pub xs: Vec<f32>,
    pub ys: Vec<f32>,
    pub rxs: Vec<f32>,
    pub rys: Vec<f32>,
    pub vscroll: f32,
    pub hscroll: f32,
    pub cursor_physical_x: f32,
    pub cursor_physical_y: f32,
}

static EVENT_TX: Mutex<Option<mpsc::UnboundedSender<CapturedEvent>>> = Mutex::new(None);

/// Per-window hold-drag flag visible to Kotlin via
/// `Java_com_zdroid_NativeBridge_isHoldDragActive`. Kotlin reads on
/// every multi-touch MOVE to decide whether to update its on-screen
/// cursor sprite. During hold-and-drag the cursor follows the moving
/// finger (so the user sees the selection growing); during plain
/// two-finger scroll the cursor stays pinned. Rust flips per-window
/// on hold-drag entry and clears on teardown.
static HOLD_DRAG_ACTIVE_PER_WINDOW: Mutex<Option<HashMap<u64, bool>>> = Mutex::new(None);

fn set_hold_drag_active(window_id: u64, active: bool) {
    let mut guard = HOLD_DRAG_ACTIVE_PER_WINDOW.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if active {
        map.insert(window_id, true);
    } else {
        map.remove(&window_id);
    }
}

fn is_hold_drag_active(window_id: u64) -> bool {
    HOLD_DRAG_ACTIVE_PER_WINDOW
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(&window_id).copied())
        .unwrap_or(false)
}

/// Construct a fresh sender/receiver pair. Returns the receiver for
/// the platform to drain; the sender lives in `EVENT_TX` for the JNI
/// thread to push into. Safe to call multiple times (Activity-
/// recreation idempotent) — each call drops the previous sender.
pub(crate) fn init_event_channel() -> mpsc::UnboundedReceiver<CapturedEvent> {
    let (tx, rx) = mpsc::unbounded();
    *EVENT_TX.lock().unwrap() = Some(tx);
    rx
}

/// Captured-pointer gesture state. Lives as a field on
/// `AndroidWindowState` so each window's gesture (drag-lock timers,
/// hold-drag cursors, tap-pending flags) is fully isolated from
/// gestures in other windows. Cursor position is owned by Kotlin and
/// threaded through every event so the on-screen cursor View and the
/// gpui-side cursor never drift.
#[derive(Default)]
pub(crate) struct CapturedPointerState {
    /// Set when `ACTION_BUTTON_PRESS` fired; we hold the button until
    /// `ACTION_BUTTON_RELEASE`. Used to populate `pressed_button` on
    /// subsequent move events.
    button_held: Option<MouseButton>,
    /// Latched on `ACTION_DOWN` (first finger). Cleared on Up, Cancel,
    /// or when motion exceeds `TAP_MOTION_PX`. Used to synthesize a
    /// click on a clean Down→Up with minimal motion.
    primary_down: Option<TapAnchor>,
    /// When the first finger of the current gesture landed. Distinct
    /// from `primary_down` because the latter clears on small cursor
    /// motion (to prevent tap-on-up after a drag) but this stays
    /// stable through the whole gesture so drag-lock detection can
    /// measure how long the first finger has been on the pad before
    /// the second one landed. Cleared only on UP / CANCEL.
    first_finger_at: Option<Instant>,
    /// When the most recent non-zero motion delta arrived (any
    /// pointer count, any finger). Used to gate hold-and-drag entry:
    /// finger 2 only triggers the gesture if finger 1 has been still
    /// for `HOLD_DRAG_STATIONARY`. Cleared on DOWN / UP / CANCEL.
    last_motion_at: Option<Instant>,
    /// Accumulated cursor motion since the last tap anchor was set.
    /// Compared to `TAP_MOTION_PX` to decide whether a Down→Up
    /// sequence is a tap or a drag.
    motion_accum: f32,
    /// True when more than one finger is currently on the pad, so
    /// motion events synthesize scroll instead of cursor movement.
    in_multi_touch: bool,
    /// Set when `ACTION_POINTER_DOWN` (second finger) landed within
    /// the two-finger-tap window without significant motion. The
    /// actual right-click fires on `ACTION_POINTER_UP` so an in-
    /// flight two-finger SCROLL (which exceeds the motion threshold
    /// before lifting) clears this and doesn't synthesize a click.
    right_click_armed: bool,
    /// Tap-tap-drag tracking: when the most recent single-finger tap
    /// ended (ACTION_UP that synthesized a click). The next
    /// ACTION_DOWN within `TAP_DRAG_WINDOW` of this point near
    /// `last_tap_position` arms the drag.
    last_tap_at: Option<Instant>,
    /// Position of the most recent tap (in logical pixels). The
    /// second-tap ACTION_DOWN must land within `TAP_DRAG_POSITION_SLOP_PX`
    /// of this to qualify as part of a tap-tap-drag gesture.
    last_tap_position: Point<Pixels>,
    /// True between the second-tap ACTION_DOWN and the first MOVE
    /// past `TAP_DRAG_SLOP_PX`. If the user lifts the finger without
    /// moving (a plain double-tap), this clears and the second tap
    /// just becomes a second click. If they move past the slop, we
    /// commit to drag mode.
    tap_drag_pending: bool,
    /// True after either of the two drag-entry paths fires:
    ///   - tap-tap-drag's `tap_drag_pending` resolved past the motion
    ///     slop (single finger, second tap-and-drag), OR
    ///   - hold-and-drag fired on `ACTION_POINTER_DOWN` (finger 1 held
    ///     past `HOLD_DRAG_THRESHOLD` before finger 2 landed).
    /// Either way `MouseDown(Left)` is already emitted and we are in
    /// drag mode; subsequent MouseMove fires with Left held until UP /
    /// POINTER_UP, which emits the matching MouseUp.
    drag_active: bool,
    /// Set to `Some` while in hold-and-drag mode (drag_active &&
    /// engaged via two-finger entry). The visible cursor (Kotlin's)
    /// stays pinned at finger 1's anchor during multi-touch, so we
    /// can't read motion from `cursor` like we do for tap-tap-drag
    /// (single-finger, where Kotlin moves the cursor). Instead we
    /// accumulate finger 2's `AXIS_RELATIVE_X/Y` deltas into this
    /// cursor and emit `MouseMove` at it so the editor's selection
    /// grows in real time. `None` for tap-tap-drag drags (the regular
    /// `cursor` is accurate there).
    hold_drag_cursor: Option<Point<Pixels>>,
    /// libinput-style drag lock. Set when a tap-tap-drag ACTION_UP
    /// fires but we want to keep `button_held` pressed for the
    /// `DRAG_LOCK_TIMEOUT` window so a fresh swipe can continue the
    /// drag. A new ACTION_DOWN within the window clears this and
    /// continues; a deliberate tap (DOWN then UP with low motion
    /// inside `DRAG_LOCK_TAP_WINDOW`) ends the drag; the timeout
    /// auto-ends on the next event. Only set for single-finger
    /// tap-tap-drag (not for hold-and-drag, which is intrinsically
    /// multi-touch and ends on full lift).
    drag_lock_pending_at: Option<Instant>,
    /// When the most recent ACTION_DOWN landed while
    /// `drag_lock_pending_at` was set. Used by the matching
    /// ACTION_UP to decide tap-vs-swipe: a brief DOWN+UP under
    /// `DRAG_LOCK_TAP_WINDOW` with low motion ends the drag;
    /// otherwise the drag continues and we re-arm
    /// `drag_lock_pending_at` again on UP.
    drag_lock_resume_at: Option<Instant>,
    /// Last cursor position seen, in logical pixels. Kept so we can
    /// emit `MouseUp` / synthetic-tap events at the correct location
    /// even when the triggering event itself doesn't carry an updated
    /// cursor (e.g. `ACTION_BUTTON_RELEASE` may arrive without a
    /// fresh relative delta).
    last_cursor: Point<Pixels>,
}

struct TapAnchor {
    when: Instant,
    cursor: Point<Pixels>,
}

/// Push a captured event from the JVM listener thread onto the channel.
/// The game thread drains and synthesizes.
fn dispatch(event: CapturedEvent) {
    // Rust-side arrival probe. Mirrors Kotlin's `M/act.gen` lines so a
    // diagnostic dump shows whether JNI translation faithfully forwarded
    // every captured event the View received. Empty Rust ring + populated
    // Kotlin ring = JNI bridge dropped events. Populated both sides =
    // arrival is fine, look downstream at the SM.
    let rx0 = event.rxs.first().copied().unwrap_or(0.0);
    let ry0 = event.rys.first().copied().unwrap_or(0.0);
    crate::diagnostic::record(format!(
        "cap.disp w={} act=0x{:x} src=0x{:x} btn=0x{:x} n={} rx={:.2} ry={:.2}",
        event.window_id,
        event.action_masked,
        event.source,
        event.button_state,
        event.pointer_count,
        rx0,
        ry0,
    ));
    let guard = EVENT_TX.lock().unwrap();
    let Some(tx) = guard.as_ref() else {
        log::warn!("captured_pointer: event arrived before init_event_channel");
        return;
    };
    if let Err(err) = tx.unbounded_send(event) {
        log::warn!("captured_pointer: dispatch failed: {err:#}");
    }
}

/// Translate one captured event into zero or more `PlatformInput`s,
/// updating internal state. Called on the game thread from the
/// platform's main loop drain. `scale_factor` is the surface's
/// physical-to-logical multiplier; we divide Kotlin's physical-pixel
/// cursor position by it to get logical pixels for gpui events.
pub(crate) fn translate(
    state: &mut CapturedPointerState,
    event: CapturedEvent,
    scale_factor: f32,
) -> Vec<PlatformInput> {
    let modifiers = Modifiers::default();
    let pressed_mouse_button = button_from_state(event.button_state);
    let scale = if scale_factor > 0.0 { scale_factor } else { 1.0 };
    let cursor = point(
        px(event.cursor_physical_x / scale),
        px(event.cursor_physical_y / scale),
    );

    let window_id = event.window_id;
    state.last_cursor = cursor;
    let mut out = Vec::new();

        match event.action_masked {
            JAVA_ACTION_DOWN => {
                // First finger landed. Four roles in priority order:
                //
                // 1. Drag-lock resume: if the previous tap-tap-drag's
                //    ACTION_UP put us in drag-lock-pending and we're
                //    still inside `DRAG_LOCK_TIMEOUT`, this DOWN
                //    continues the drag instead of starting fresh.
                //    Button stays held (no new MouseDown emit). The
                //    matching ACTION_UP decides tap-vs-swipe.
                // 2. Defensive cleanup: if previous gesture left
                //    `drag_active` or `button_held` set without an
                //    in-flight drag lock, this is leaked state —
                //    emit MouseUp and clear.
                // 3. Tap-tap-drag detection: if this DOWN follows a
                //    recent tap near the same position, arm
                //    `tap_drag_pending` so the next MOVE past slop
                //    commits to drag mode.
                // 4. Single-tap detection: latch the anchor so the
                //    matching UP can synthesize a click.
                let now = Instant::now();
                let drag_lock_active = state.drag_lock_pending_at
                    .map(|t| now.duration_since(t) <= DRAG_LOCK_TIMEOUT)
                    .unwrap_or(false);
                if drag_lock_active {
                    log::info!(
                        "drag_lock: RESUME elapsed_ms={}",
                        state.drag_lock_pending_at
                            .map(|t| now.duration_since(t).as_millis())
                            .unwrap_or(0),
                    );
                    state.drag_lock_pending_at = None;
                    state.drag_lock_resume_at = Some(now);
                    // button_held + drag_active stay set; cursor
                    // continues from wherever it is. tap-tap-drag
                    // pending is cleared so this DOWN doesn't also
                    // trigger another tap-tap-drag commit on top.
                    state.tap_drag_pending = false;
                    state.last_tap_at = None;
                    state.primary_down = Some(TapAnchor { when: now, cursor });
                    state.first_finger_at = Some(now);
                    state.last_motion_at = None;
                    state.motion_accum = 0.0;
                    state.in_multi_touch = false;
                    state.right_click_armed = false;
                    return out;
                }
                if state.drag_active || state.button_held.is_some() {
                    log::info!(
                        "captured_pointer: stale drag state cleared on DOWN \
                         (drag_active={} button_held={:?} hold_drag_cursor={} \
                         drag_lock_pending={})",
                        state.drag_active,
                        state.button_held,
                        state.hold_drag_cursor.is_some(),
                        state.drag_lock_pending_at.is_some(),
                    );
                    let release_pos = state.hold_drag_cursor.take().unwrap_or(state.last_cursor);
                    if let Some(button) = state.button_held.take() {
                        out.push(PlatformInput::MouseUp(MouseUpEvent {
                            button,
                            position: release_pos,
                            modifiers,
                            click_count: 1,
                        }));
                    }
                    state.drag_active = false;
                    state.drag_lock_pending_at = None;
                    state.drag_lock_resume_at = None;
                    set_hold_drag_active(window_id, false);
                }
                let now = Instant::now();
                let qualifies_as_tap_drag = state
                    .last_tap_at
                    .map(|t| now.duration_since(t) < TAP_DRAG_WINDOW)
                    .unwrap_or(false)
                    && (cursor - state.last_tap_position).magnitude()
                        <= TAP_DRAG_POSITION_SLOP_PX as f64;
                state.tap_drag_pending = qualifies_as_tap_drag;
                state.last_tap_at = None;
                state.primary_down = Some(TapAnchor {
                    when: now,
                    cursor,
                });
                state.first_finger_at = Some(now);
                state.last_motion_at = None;
                state.motion_accum = 0.0;
                state.in_multi_touch = false;
                state.right_click_armed = false;
            }
            JAVA_ACTION_POINTER_DOWN => {
                // Second-or-later finger landed. Three sub-cases:
                //
                // 1. We are ALREADY in hold-and-drag mode
                //    (drag_active && hold_drag_cursor.is_some()):
                //    this is finger 2 re-landing after the trackpad
                //    momentarily dropped its contact. Do NOT
                //    re-engage. Don't fire another MouseDown. Just
                //    keep multi-touch state set so subsequent MOVE
                //    events continue to grow the existing drag.
                // 2. Conditions for hold-and-drag entry met: enter
                //    drag mode at the current cursor.
                // 3. Neither: scroll territory. Arm right-click.
                let now = Instant::now();
                let hold_elapsed = state
                    .first_finger_at
                    .map(|t| now.duration_since(t))
                    .unwrap_or_default();
                let stationary_for = state
                    .last_motion_at
                    .map(|t| now.duration_since(t))
                    .unwrap_or(Duration::MAX);
                state.primary_down = None;
                state.in_multi_touch = true;
                state.tap_drag_pending = false;
                if state.drag_active && state.hold_drag_cursor.is_some() {
                    // Already in hold-drag — finger 2 contact wavered
                    // and re-landed. Keep going, don't re-emit.
                    log::info!(
                        "hold_drag: CONTINUE (finger re-landed during drag, \
                         drag_cursor=({:.0},{:.0}))",
                        f32::from(
                            state
                                .hold_drag_cursor
                                .map(|c| c.x)
                                .unwrap_or(Pixels::ZERO),
                        ),
                        f32::from(
                            state
                                .hold_drag_cursor
                                .map(|c| c.y)
                                .unwrap_or(Pixels::ZERO),
                        ),
                    );
                } else if hold_elapsed >= HOLD_DRAG_THRESHOLD
                    && stationary_for >= HOLD_DRAG_STATIONARY
                    && !state.drag_active
                {
                    log::info!(
                        "hold_drag: ENGAGED anchor=({:.0},{:.0}) hold_ms={} stationary_ms={}",
                        f32::from(cursor.x),
                        f32::from(cursor.y),
                        hold_elapsed.as_millis(),
                        stationary_for.as_millis().min(99999),
                    );
                    state.drag_active = true;
                    state.hold_drag_cursor = Some(cursor);
                    state.right_click_armed = false;
                    state.button_held = Some(MouseButton::Left);
                    set_hold_drag_active(window_id, true);
                    out.push(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: cursor,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }));
                } else {
                    log::info!(
                        "hold_drag: SKIPPED hold_ms={} stationary_ms={} \
                         drag_active={} button_held={:?} (scroll path)",
                        hold_elapsed.as_millis(),
                        stationary_for.as_millis().min(99999),
                        state.drag_active,
                        state.button_held,
                    );
                    state.right_click_armed = true;
                    // Verified empirically: the Book Cover trackpad's
                    // ACTION_BUTTON_PRESS fires shortly after finger 1
                    // DOWN regardless of user intent. If it already
                    // emitted a MouseDown, the editor sees scroll-
                    // while-clicked when this scroll path proceeds —
                    // which most editors interpret as drag-select
                    // (visible to the user as "scroll triggers
                    // selection"). Release the button first so the
                    // scroll is independent of the firmware-emitted
                    // click.
                    if let Some(button) = state.button_held.take() {
                        log::info!(
                            "hold_drag: releasing inherited BUTTON_PRESS before scroll \
                             (button={:?})",
                            button,
                        );
                        out.push(PlatformInput::MouseUp(MouseUpEvent {
                            button,
                            position: cursor,
                            modifiers,
                            click_count: 1,
                        }));
                    }
                }
            }
            JAVA_ACTION_MOVE => {
                if event.pointer_count >= 2 || state.in_multi_touch {
                    if let Some(mut drag_cursor) = state.hold_drag_cursor {
                        // Hold-and-drag in progress: route motion as
                        // drag-MouseMove, NOT scroll. Sum of relative
                        // deltas approximates finger 2's motion since
                        // finger 1 is held in place; if finger 1 does
                        // drift slightly the sum still tracks the
                        // intended drag direction. Update the
                        // internal drag cursor and emit
                        // `MouseMove(Left held)` at the new position
                        // so the editor selection grows in real time.
                        let sum_rx: f32 = event.rxs.iter().sum();
                        let sum_ry: f32 = event.rys.iter().sum();
                        let dx = sum_rx / scale;
                        let dy = sum_ry / scale;
                        drag_cursor.x += px(dx);
                        drag_cursor.y += px(dy);
                        state.hold_drag_cursor = Some(drag_cursor);
                        if dx != 0.0 || dy != 0.0 {
                            state.last_motion_at = Some(Instant::now());
                            out.push(PlatformInput::MouseMove(MouseMoveEvent {
                                position: drag_cursor,
                                pressed_button: state.button_held,
                                modifiers,
                            }));
                        }
                    } else {
                        // Plain two-finger scroll (no hold-drag in
                        // flight). Centroid delta drives
                        // ScrollDelta::Pixels. Sign matches
                        // `events/trackpad.rs` (the non-captured path)
                        // so behavior is consistent whether or not
                        // capture is engaged.
                        let n = event.pointer_count.max(1) as f32;
                        let sum_rx: f32 = event.rxs.iter().sum();
                        let sum_ry: f32 = event.rys.iter().sum();
                        let dx = sum_rx / n / scale;
                        let dy = sum_ry / n / scale;
                        // Any meaningful motion disarms right-click
                        // (gesture is no longer a quick two-finger
                        // tap).
                        if (dx * dx + dy * dy).sqrt() > TWO_FINGER_TAP_MOTION_PX {
                            state.right_click_armed = false;
                        }
                        if dx != 0.0 || dy != 0.0 {
                            state.last_motion_at = Some(Instant::now());
                            out.push(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position: cursor,
                                delta: ScrollDelta::Pixels(point(px(dx), px(dy))),
                                modifiers,
                                touch_phase: TouchPhase::Moved,
                            }));
                        }
                    }
                } else if event.pointer_count == 1 {
                    // Single-finger move. Cursor is already at the
                    // post-delta position (Kotlin updated it before
                    // forwarding). Track motion accumulator so tap
                    // synthesis gets cancelled if the finger drifts
                    // past the tap threshold.
                    let dx = *event.rxs.first().unwrap_or(&0.0);
                    let dy = *event.rys.first().unwrap_or(&0.0);
                    let mag = (dx * dx + dy * dy).sqrt();
                    state.motion_accum += mag;
                    if mag > 0.0 {
                        state.last_motion_at = Some(Instant::now());
                    }
                    if state.motion_accum > TAP_MOTION_PX {
                        state.primary_down = None;
                    }

                    // Tap-tap-drag commit: if a previous tap armed
                    // `tap_drag_pending` on this DOWN, the first motion
                    // past slop transitions us into drag mode. Emit
                    // MouseDown(Left) at the original tap position
                    // (the anchor, where the user expected the drag
                    // to start from) and subsequent MouseMove events
                    // grow the drag.
                    if state.tap_drag_pending
                        && state.motion_accum > TAP_DRAG_SLOP_PX
                        && state.button_held.is_none()
                    {
                        log::info!(
                            "tap_drag: ENGAGED anchor=({:.0},{:.0}) cursor=({:.0},{:.0})",
                            f32::from(state.last_tap_position.x),
                            f32::from(state.last_tap_position.y),
                            f32::from(cursor.x),
                            f32::from(cursor.y),
                        );
                        state.tap_drag_pending = false;
                        state.drag_active = true;
                        state.hold_drag_cursor = None;
                        state.button_held = Some(MouseButton::Left);
                        out.push(PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: state.last_tap_position,
                            modifiers,
                            click_count: 1,
                            first_mouse: false,
                        }));
                    }

                    out.push(PlatformInput::MouseMove(MouseMoveEvent {
                        position: cursor,
                        pressed_button: state.button_held,
                        modifiers,
                    }));
                }
            }
            JAVA_ACTION_UP => {
                // Last finger lifted. Four cases:
                //
                // 1. Drag-lock resume tap: a fresh ACTION_DOWN earlier
                //    set `drag_lock_resume_at`. If this UP comes
                //    within DRAG_LOCK_TAP_WINDOW and motion stayed
                //    under TAP_MOTION_PX, the user did a deliberate
                //    tap to END the drag. Emit MouseUp.
                // 2. Drag-lock resume swipe: the user is mid-drag,
                //    motion happened. Re-arm drag-lock-pending (keep
                //    button held; another swipe can continue from
                //    here).
                // 3. button_held set, no drag-lock in flight: this is
                //    the end of a tap-tap-drag or hold-and-drag.
                //    For tap-tap-drag (single-finger,
                //    hold_drag_cursor==None), enter drag-lock-pending
                //    instead of releasing — the user can swipe
                //    again to continue selecting. Hold-and-drag
                //    (multi-touch with hold_drag_cursor) ends
                //    normally.
                // 4. Plain tap (primary_down anchor fresh, low
                //    motion): synthesize a click.
                state.in_multi_touch = false;
                set_hold_drag_active(window_id, false);
                let now = Instant::now();
                if let Some(resume_at) = state.drag_lock_resume_at.take() {
                    let resume_elapsed = now.duration_since(resume_at);
                    let is_tap = resume_elapsed < DRAG_LOCK_TAP_WINDOW
                        && state.motion_accum <= TAP_MOTION_PX;
                    if is_tap {
                        // User tapped without dragging — end the
                        // drag.
                        if let Some(button) = state.button_held.take() {
                            let release_pos = state.hold_drag_cursor.take().unwrap_or(cursor);
                            log::info!(
                                "drag_lock: ENDED by tap at ({:.0},{:.0}) tap_ms={}",
                                f32::from(cursor.x),
                                f32::from(cursor.y),
                                resume_elapsed.as_millis(),
                            );
                            out.push(PlatformInput::MouseUp(MouseUpEvent {
                                button,
                                position: release_pos,
                                modifiers,
                                click_count: 1,
                            }));
                        }
                        state.drag_active = false;
                        state.primary_down = None;
                        state.motion_accum = 0.0;
                        state.first_finger_at = None;
                        state.last_motion_at = None;
                        state.tap_drag_pending = false;
                        return out;
                    }
                    // Swipe (with motion) — keep drag alive and
                    // re-arm the lock so another swipe can continue.
                    log::info!(
                        "drag_lock: RE-ARM after swipe (motion_px={:.1})",
                        state.motion_accum,
                    );
                    state.drag_lock_pending_at = Some(now);
                    state.motion_accum = 0.0;
                    state.primary_down = None;
                    state.first_finger_at = None;
                    state.last_motion_at = None;
                    state.tap_drag_pending = false;
                    return out;
                }
                if let Some(button) = state.button_held.take() {
                    // Single-finger tap-tap-drag → enter drag-lock.
                    // Hold-and-drag → end normally.
                    let is_tap_tap_drag =
                        state.drag_active && state.hold_drag_cursor.is_none();
                    if is_tap_tap_drag {
                        log::info!(
                            "drag_lock: ARMED at ({:.0},{:.0})",
                            f32::from(cursor.x),
                            f32::from(cursor.y),
                        );
                        // Restore button_held; we kept it held in
                        // anticipation of a swipe-continue.
                        state.button_held = Some(button);
                        state.drag_lock_pending_at = Some(now);
                        state.primary_down = None;
                        state.motion_accum = 0.0;
                        state.first_finger_at = None;
                        state.last_motion_at = None;
                        state.tap_drag_pending = false;
                        return out;
                    }
                    let release_pos = state.hold_drag_cursor.take().unwrap_or(cursor);
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position: release_pos,
                        modifiers,
                        click_count: 1,
                    }));
                    state.drag_active = false;
                } else if let Some(anchor) = state.primary_down.take()
                    && anchor.when.elapsed() < TAP_WINDOW
                    && state.motion_accum <= TAP_MOTION_PX
                {
                    out.push(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: cursor,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }));
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: cursor,
                        modifiers,
                        click_count: 1,
                    }));
                    // Record this tap so a subsequent ACTION_DOWN
                    // within `TAP_DRAG_WINDOW` can arm tap-tap-drag
                    // for text selection.
                    state.last_tap_at = Some(Instant::now());
                    state.last_tap_position = cursor;
                }
                state.motion_accum = 0.0;
                state.right_click_armed = false;
                state.first_finger_at = None;
                state.last_motion_at = None;
                state.tap_drag_pending = false;
            }
            JAVA_ACTION_POINTER_UP => {
                // A non-primary finger lifted. CRITICAL: do not tear
                // down hold-and-drag here. The Book Cover trackpad's
                // finger-2 contact wavers during sustained gestures
                // (verified empirically: same-anchor hold_drag ENGAGED
                // fires repeatedly through POINTER_UP/POINTER_DOWN
                // cycles during one continuous user drag). If we tear
                // down on POINTER_UP, the next POINTER_DOWN re-engages
                // hold-drag and re-fires MouseDown, producing the
                // click-storm. Only ACTION_UP (last finger lifted)
                // ends the gesture.
                //
                // Two things still need to happen here:
                //   - Fire right-click if armed and motion stayed
                //     under the tap threshold (genuine two-finger tap)
                //   - When pointer_count drops to 1, clear
                //     in_multi_touch so subsequent single-finger MOVE
                //     events route through the cursor path
                if state.right_click_armed && !state.drag_active {
                    out.push(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Right,
                        position: cursor,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }));
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Right,
                        position: cursor,
                        modifiers,
                        click_count: 1,
                    }));
                    state.right_click_armed = false;
                }
                // Keep in_multi_touch true while hold-drag is active
                // (finger 2 wavering doesn't end the gesture; only
                // ACTION_UP does). Clear it otherwise so single-finger
                // motion routes through the cursor path, not scroll.
                if event.pointer_count <= 2 && state.hold_drag_cursor.is_none() {
                    state.in_multi_touch = false;
                }
            }
            JAVA_ACTION_BUTTON_PRESS => {
                // SOURCE_TOUCHPAD BUTTON_PRESS is firmware noise on
                // the Book Cover trackpad (logs verify it fires on
                // every finger contact, regardless of click intent).
                // Honoring it as a click breaks the gesture state
                // machine: it sets button_held mid-gesture, blocks
                // hold-and-drag entry, and combines with two-finger
                // scroll to produce "scroll triggers selection" in
                // the editor. Ignore for touchpad source; for real
                // mouse buttons (SOURCE_MOUSE), the event represents
                // a genuine click and must be honored.
                if event.source & ANDROID_SOURCE_TOUCHPAD == ANDROID_SOURCE_TOUCHPAD {
                    // touchpad noise: drop on the floor
                } else {
                    let button = pressed_mouse_button.unwrap_or(MouseButton::Left);
                    state.button_held = Some(button);
                    state.primary_down = None;
                    out.push(PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position: cursor,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }));
                }
            }
            JAVA_ACTION_BUTTON_RELEASE => {
                if event.source & ANDROID_SOURCE_TOUCHPAD == ANDROID_SOURCE_TOUCHPAD {
                    // touchpad noise: drop on the floor (matches PRESS)
                } else if let Some(button) = state.button_held.take() {
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position: cursor,
                        modifiers,
                        click_count: 1,
                    }));
                }
            }
            JAVA_ACTION_CANCEL => {
                if let Some(button) = state.button_held.take() {
                    let release_pos = state.hold_drag_cursor.take().unwrap_or(cursor);
                    out.push(PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position: release_pos,
                        modifiers,
                        click_count: 0,
                    }));
                }
                state.primary_down = None;
                state.first_finger_at = None;
                state.last_motion_at = None;
                state.motion_accum = 0.0;
                state.in_multi_touch = false;
                state.right_click_armed = false;
                state.tap_drag_pending = false;
                state.drag_active = false;
                state.hold_drag_cursor = None;
                state.drag_lock_pending_at = None;
                state.drag_lock_resume_at = None;
                set_hold_drag_active(window_id, false);
            }
        _ => {}
    }
    out
}

fn button_from_state(state: i32) -> Option<MouseButton> {
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

// JNI sinks --------------------------------------------------------------

/// Kotlin queries this on every multi-touch MOVE to decide whether to
/// move the on-screen cursor sprite. `true` while a hold-and-drag is
/// in flight on the given window; `false` during plain scroll or
/// single-finger gestures. Per-window so spawned windows have their
/// own independent flag. Pass `0` (`PRIMARY_WINDOW_ID`) for
/// MainActivity, the extra window's id for spawned windows.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_isHoldDragActive<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: jni::sys::jlong,
) -> bool {
    is_hold_drag_active(window_id as u64)
}

/// Probe sink (kept alongside the structured one for debug). Logs a
/// stringified summary of each captured event.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnCapturedPointerProbe<
    'local,
>(
    mut env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    summary: JString<'local>,
) {
    let summary: String = match env.get_string(&summary) {
        Ok(s) => s.into(),
        Err(err) => {
            log::warn!("captured_pointer: failed to decode summary: {err:#}");
            return;
        }
    };
    log::info!("captured_pointer: {summary}");
}

/// Structured sink for the primary `MainActivity` window. Marshals
/// Kotlin-side `MotionEvent` fields, builds a `CapturedEvent` tagged
/// with `PRIMARY_WINDOW_ID`, dispatches onto the channel for the game
/// thread to drain. Spawned `ExtraWindowActivity` windows use
/// `nativeOnExtraCapturedPointer` instead.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnCapturedPointer<
    'local,
>(
    env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    action_masked: i32,
    source: i32,
    button_state: i32,
    pointer_count: i32,
    xs: JFloatArray<'local>,
    ys: JFloatArray<'local>,
    rxs: JFloatArray<'local>,
    rys: JFloatArray<'local>,
    vscroll: f32,
    hscroll: f32,
    cursor_physical_x: f32,
    cursor_physical_y: f32,
) {
    let n = pointer_count.max(0) as usize;
    let xs = read_floats(&env, &xs, n);
    let ys = read_floats(&env, &ys, n);
    let rxs = read_floats(&env, &rxs, n);
    let rys = read_floats(&env, &rys, n);
    dispatch(CapturedEvent {
        window_id: PRIMARY_WINDOW_ID,
        action_masked,
        source,
        button_state,
        pointer_count: n,
        xs,
        ys,
        rxs,
        rys,
        vscroll,
        hscroll,
        cursor_physical_x,
        cursor_physical_y,
    });
}

/// Structured sink for spawned `ExtraWindowActivity` windows. Same
/// payload as `nativeOnCapturedPointer` plus a `windowId` so the
/// per-window state machine routes the event to the right
/// `SynthState`. Extra windows call this from their
/// `onGenericMotionEvent` override; gesture pipelines (tap, scroll,
/// hold-drag, drag-lock, cursor follow) are identical per window.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraCapturedPointer<
    'local,
>(
    env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: jni::sys::jlong,
    action_masked: i32,
    source: i32,
    button_state: i32,
    pointer_count: i32,
    xs: JFloatArray<'local>,
    ys: JFloatArray<'local>,
    rxs: JFloatArray<'local>,
    rys: JFloatArray<'local>,
    vscroll: f32,
    hscroll: f32,
    cursor_physical_x: f32,
    cursor_physical_y: f32,
) {
    let n = pointer_count.max(0) as usize;
    let xs = read_floats(&env, &xs, n);
    let ys = read_floats(&env, &ys, n);
    let rxs = read_floats(&env, &rxs, n);
    let rys = read_floats(&env, &rys, n);
    dispatch(CapturedEvent {
        window_id: window_id as u64,
        action_masked,
        source,
        button_state,
        pointer_count: n,
        xs,
        ys,
        rxs,
        rys,
        vscroll,
        hscroll,
        cursor_physical_x,
        cursor_physical_y,
    });
}

fn read_floats<'local>(
    env: &jni::JNIEnv<'local>,
    array: &JFloatArray<'local>,
    expected_len: usize,
) -> Vec<f32> {
    let mut buf = vec![0.0f32; expected_len];
    if expected_len == 0 {
        return buf;
    }
    if let Err(err) = env.get_float_array_region(array, 0, buf.as_mut_slice()) {
        log::warn!("captured_pointer: get_float_array_region: {err:#}");
        buf.clear();
    }
    buf
}
