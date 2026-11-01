//! Android's thermal status, for gpui, which caps frames at 60 a second while
//! it's serious or critical. A long build can heat the device until Android
//! throttles it hard.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use gpui::ThermalState;

// `AThermalStatus` in android/thermal.h.
const STATUS_LIGHT: i32 = 1;
const STATUS_MODERATE: i32 = 2;
const STATUS_SEVERE: i32 = 3;

#[link(name = "android")]
unsafe extern "C" {
    fn AThermal_acquireManager() -> *mut c_void;
    fn AThermal_getCurrentThermalStatus(manager: *mut c_void) -> i32;
    fn AThermal_registerThermalStatusListener(
        manager: *mut c_void,
        callback: unsafe extern "C" fn(data: *mut c_void, status: i32),
        data: *mut c_void,
    ) -> i32;
}

static STATUS: AtomicI32 = AtomicI32::new(0);
static CHANGED: AtomicBool = AtomicBool::new(false);

/// Runs on a binder thread.
unsafe extern "C" fn status_changed(_data: *mut c_void, status: i32) {
    STATUS.store(status, Ordering::Release);
    CHANGED.store(true, Ordering::Release);
    crate::platform::wake_run_loop();
}

/// Starts following the status. The manager lives as long as the process.
pub(crate) fn start() {
    unsafe {
        let manager = AThermal_acquireManager();
        if manager.is_null() {
            log::warn!("thermal: no thermal manager");
            return;
        }
        STATUS.store(AThermal_getCurrentThermalStatus(manager), Ordering::Release);
        let result =
            AThermal_registerThermalStatusListener(manager, status_changed, std::ptr::null_mut());
        if result != 0 {
            log::warn!("thermal: registering the listener failed with {result}");
        }
    }
}

/// `ERROR`, -1, and `NONE` count as nominal.
pub(crate) fn state() -> ThermalState {
    match STATUS.load(Ordering::Acquire) {
        STATUS_LIGHT | STATUS_MODERATE => ThermalState::Fair,
        STATUS_SEVERE => ThermalState::Serious,
        status if status > STATUS_SEVERE => ThermalState::Critical,
        _ => ThermalState::Nominal,
    }
}

/// Whether the status changed since the last call.
pub(crate) fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}
