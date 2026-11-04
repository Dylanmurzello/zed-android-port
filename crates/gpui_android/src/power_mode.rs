//! Android's Battery Saver, for gpui, which caps animation frames at 30 a
//! second in [`PowerMode::LowPower`]. `MainActivity` reports
//! `PowerManager.isPowerSaveMode()` when it starts and on every
//! `ACTION_POWER_SAVE_MODE_CHANGED`.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::PowerMode;
use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::jboolean;

static POWER_SAVE: AtomicBool = AtomicBool::new(false);
static CHANGED: AtomicBool = AtomicBool::new(false);

/// Android only says whether Battery Saver is on, so any other state is
/// `Unknown`, as gpui's trait asks of a platform that can't tell the others
/// apart.
pub(crate) fn mode() -> PowerMode {
    if POWER_SAVE.load(Ordering::Acquire) {
        PowerMode::LowPower
    } else {
        PowerMode::Unknown
    }
}

/// Whether the mode changed since the last call.
pub(crate) fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}

/// `NativeBridge.nativePowerSaveModeChanged`, on Android's main thread. The
/// first report can arrive before or after gpui reads the mode at startup.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativePowerSaveModeChanged<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    power_save: jboolean,
) {
    let power_save = power_save != 0;
    if POWER_SAVE.swap(power_save, Ordering::AcqRel) != power_save {
        log::info!(
            "power_mode: Battery Saver {}",
            if power_save { "on" } else { "off" }
        );
        CHANGED.store(true, Ordering::Release);
        crate::platform::wake_run_loop();
    }
}
