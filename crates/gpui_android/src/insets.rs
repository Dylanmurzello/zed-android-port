//! The insets each window's Activity reports through
//! `NativeBridge.nativeOnWindowInsetsChanged`: the system bars and display
//! cutout, and the on-screen keyboard with the programming extras row on it.
//! `AndroidWindow` lays gpui out above the keyboard and the row from them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use jni::objects::JObject;

/// One window's insets in physical pixels, as its Activity reported them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PhysicalInsets {
    /// `WindowInsets` types `systemBars() | displayCutout()`, zero where
    /// the system bars are hidden.
    pub(crate) safe_area_left: i32,
    pub(crate) safe_area_top: i32,
    pub(crate) safe_area_right: i32,
    pub(crate) safe_area_bottom: i32,
    /// How far the on-screen keyboard (`WindowInsets.Type.ime()`), and the
    /// programming extras row while it's shown on the keyboard, reach up
    /// from the window's bottom edge; every frame while the keyboard slides.
    pub(crate) obscured_bottom: i32,
}

/// The latest insets of each window, by window id (`PRIMARY_WINDOW_ID` for
/// `MainActivity`). The latest value rather than a queue: an Activity
/// reports before Rust has opened its window, and only the latest matters.
static REPORTED: Mutex<BTreeMap<u64, PhysicalInsets>> = Mutex::new(BTreeMap::new());

/// Set by each report and cleared by [`take_changed`].
static CHANGED: AtomicBool = AtomicBool::new(false);

fn reported_insets() -> MutexGuard<'static, BTreeMap<u64, PhysicalInsets>> {
    // Inserting or removing an entry can't leave the map half-updated.
    REPORTED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The insets last reported for a window: zero until its Activity reports.
pub(crate) fn reported(window_id: u64) -> PhysicalInsets {
    reported_insets()
        .get(&window_id)
        .copied()
        .unwrap_or_default()
}

/// Whether any window reported insets since the last call.
pub(crate) fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}

/// Drops the insets of an extra window whose Activity is gone.
pub(crate) fn forget(window_id: u64) {
    reported_insets().remove(&window_id);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnWindowInsetsChanged<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    safe_area_left: i32,
    safe_area_top: i32,
    safe_area_right: i32,
    safe_area_bottom: i32,
    obscured_bottom: i32,
) {
    let insets = PhysicalInsets {
        safe_area_left,
        safe_area_top,
        safe_area_right,
        safe_area_bottom,
        obscured_bottom,
    };
    reported_insets().insert(window_id as u64, insets);
    CHANGED.store(true, Ordering::Release);
    crate::platform::wake_run_loop();
}
