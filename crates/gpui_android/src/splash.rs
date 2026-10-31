//! MainActivity polls `nativeIsZedReady` to fade out its splash overlay
//! after gpui's first paint.
//!
//! The poll runs on the Android UI thread and the flag is written on the
//! gpui thread, which GameActivity runs on a separate looper.

use std::sync::atomic::{AtomicBool, Ordering};

use jni::{JNIEnv, objects::JClass, sys::jboolean};

static ZED_READY: AtomicBool = AtomicBool::new(false);

/// Called once from the gpui-Rust side after the FIRST successful
/// window paint. Idempotent — subsequent calls are no-ops.
pub(crate) fn mark_zed_ready() {
    if !ZED_READY.swap(true, Ordering::AcqRel) {
        log::info!("zed_android::splash: marked ZED_READY=true (first paint completed)");
    }
}

/// JNI entry point for the overlay's poller. Returns 1 (true) when
/// the gpui-side has painted at least one frame, 0 (false) otherwise.
///
/// JNI signature: `()Z` — no args, returns jboolean. Kotlin shape is
/// `external fun nativeIsZedReady(): Boolean` on `NativeBridge`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeIsZedReady<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jboolean {
    if ZED_READY.load(Ordering::Acquire) {
        jni::sys::JNI_TRUE
    } else {
        jni::sys::JNI_FALSE
    }
}
