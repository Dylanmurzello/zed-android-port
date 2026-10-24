//! Opt-in background mode backed by `KeepAliveService.kt`: an ongoing
//! notification keeps the app process at foreground-service priority, so
//! terminal, SSH and agent sessions survive while Zdroid is in the
//! background. The Kotlin side persists the choice across launches.

use anyhow::Result;
use jni::objects::JValue;
use util::ResultExt as _;

pub fn is_enabled() -> bool {
    crate::updater::android_app()
        .and_then(|android_app| {
            crate::credentials::with_activity(android_app, |env, activity| {
                Ok(env
                    .call_method(activity, "isBackgroundModeEnabled", "()Z", &[])?
                    .z()?)
            })
        })
        .log_err()
        .unwrap_or(false)
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let android_app = crate::updater::android_app()?;
    crate::credentials::with_activity(android_app, |env, activity| {
        env.call_method(
            activity,
            "setBackgroundModeEnabled",
            "(Z)V",
            &[JValue::Bool(u8::from(enabled))],
        )?;
        Ok(())
    })
}
