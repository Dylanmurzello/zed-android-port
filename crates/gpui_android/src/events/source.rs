//! Input-source classification.
//!
//! Android tags every `MotionEvent` with both a device source bitmask
//! (`Source::Mouse`, `Source::Touchscreen`, …) and a per-pointer
//! `ToolType` (`Mouse`, `Stylus`, `Finger`, …). The two don't always
//! agree: a stylus on a touchscreen reports `SOURCE_TOUCHSCREEN` but
//! `tool_type=Stylus`, and a mouse on the same device shows up the
//! same way until DeX mode. `ToolType` is finer-grained, so we prefer
//! it when present and fall back to the device source.
//!
//! We collapse all of that into a 4-way [`InputSource`] enum that the
//! rest of the translator branches on. Three reasons callers care:
//!
//! 1. **Per-source multi-click windows.** A hardware mouse should match
//!    `ViewConfiguration.getDoubleTapTimeout()` (~300ms) and tight
//!    slop (≤3 logical px). Finger taps need ~500ms / ~6px because
//!    real fingers are slower and noisier than mice.
//! 2. **Hover semantics.** Touch contact should not paint mouse-hover
//!    styles. (Currently handled implicitly: Android only delivers
//!    `HOVER_*` from mouse/stylus sources, so we already bail out for
//!    touch. This classifier is here for future explicit branches.)
//! 3. **Pointer-capture decisions.** Samsung's keyboard-cover trackpad
//!    in tablet mode reports as `Source::Mouse` with relative motion;
//!    if we ever request `requestPointerCapture()`, we'll branch on
//!    `Touchpad` to enable it without affecting actual hardware mice.

use android_activity::input::{MotionEvent, Source, ToolType};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSource {
    /// Hardware mouse (USB / Bluetooth). Includes Samsung's keyboard-cover
    /// trackpad in non-DeX mode (which Android surfaces as `Source::Mouse`
    /// with relative axes).
    Mouse,
    /// Active stylus (built-in or Bluetooth) or eraser end of same.
    /// Treated identically to mouse for click timing because both are
    /// precision-aimed.
    Stylus,
    /// Standalone trackpad (rare on Android; the DeX trackpad surfaces
    /// as Mouse instead). Currently behaves like mouse; reserved as a
    /// distinct variant for the pointer-capture branch.
    Touchpad,
    /// Finger on touchscreen. Default when source/tool_type don't
    /// resolve to one of the above.
    Finger,
}

/// Classify a `MotionEvent` to [`InputSource`]. Per-pointer
/// `tool_type` takes precedence over the device source bitmask
/// because a single device (a touchscreen tablet) can deliver
/// distinct tool types in different events (finger vs. stylus vs.
/// mouse-via-USB-OTG).
pub(crate) fn classify(event: &MotionEvent) -> InputSource {
    if event.pointer_count() > 0 {
        match event.pointer_at_index(0).tool_type() {
            ToolType::Mouse => return InputSource::Mouse,
            ToolType::Stylus | ToolType::Eraser => return InputSource::Stylus,
            ToolType::Finger => return InputSource::Finger,
            _ => {}
        }
    }
    match event.source() {
        Source::Mouse | Source::MouseRelative => InputSource::Mouse,
        Source::Stylus | Source::BluetoothStylus => InputSource::Stylus,
        Source::Touchpad => InputSource::Touchpad,
        _ => InputSource::Finger,
    }
}

/// Time window in which a second / third Down counts as a double- /
/// triple-click. Matches `ViewConfiguration.getDoubleTapTimeout()` for
/// hardware (~300ms, the system default for indirect pointing) and
/// keeps the historical 500ms for finger taps (a real-world tap-tap
/// with a finger spans ~400ms at normal cadence). Tighter windows on
/// hardware make rapid double-clicks feel responsive on a wired mouse.
pub(crate) fn multi_click_window(source: InputSource) -> Duration {
    match source {
        InputSource::Mouse | InputSource::Stylus | InputSource::Touchpad => {
            Duration::from_millis(300)
        }
        InputSource::Finger => Duration::from_millis(500),
    }
}

/// Logical pixels between successive Downs before they're treated as
/// separate gestures. Mouse / stylus aim is precise, so we want a tight
/// slop or the editor will misclassify a deliberate two-spot click as
/// a single double-click. Fingers wobble: 6px covers hand-jitter on a
/// tap-tap without disqualifying intentional double-taps.
pub(crate) fn multi_click_slop(source: InputSource) -> f64 {
    match source {
        InputSource::Mouse | InputSource::Stylus => 3.0,
        InputSource::Touchpad => 4.0,
        InputSource::Finger => 6.0,
    }
}
