//! Opt-in background mode: keeps `ZdRuntimeService.kt`, the runtime's
//! foreground service, running even while the editor has no child
//! processes, so work without one, such as Zed's own agent, survives while
//! Zdroid is in the background. The Kotlin side persists the choice across
//! launches.

use anyhow::Result;
use jni::objects::JValue;
use util::ResultExt as _;

pub fn is_enabled() -> bool {
    crate::jni_env::call_main_activity_bool("isBackgroundModeEnabled")
        .log_err()
        .unwrap_or(false)
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let android_app = crate::jni_env::android_app()?;
    crate::jni_env::with_activity(android_app, |env, activity| {
        env.call_method(
            activity,
            "setBackgroundModeEnabled",
            "(Z)V",
            &[JValue::Bool(u8::from(enabled))],
        )?;
        Ok(())
    })
}
