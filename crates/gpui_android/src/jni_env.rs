//! JNI calls into the app's Java code.
//!
//! GameActivity attaches the gpui thread to the JVM for the life of the app,
//! so what a JNI call leaves behind isn't cleaned up by a detach: local
//! references pile up in the thread's reference table, and a pending Java
//! exception makes every later JNI call on the thread fail (CheckJNI aborts
//! the process). Every call goes through [`with_env`] or [`with_activity`],
//! which free the call's local references and clear its exceptions.
//!
//! What takes more than one call lives in a small Kotlin method, which this
//! then calls: the Android APIs read as they do in Android's docs, and Kotlin's
//! compiler and lint check them.

use std::sync::OnceLock;

use android_activity::AndroidApp;
use anyhow::{Context as _, Result, anyhow};
use jni::{
    JNIEnv, JavaVM,
    objects::{JObject, JString, JValue},
};
use util::ResultExt as _;

static ANDROID_APP: OnceLock<AndroidApp> = OnceLock::new();

/// Keeps the `AndroidApp` for code that runs inside the gpui `App`, with no
/// access to the one `android_main` received.
pub(crate) fn register_android_app(android_app: AndroidApp) {
    ANDROID_APP.set(android_app).ok();
}

pub(crate) fn android_app() -> Result<&'static AndroidApp> {
    ANDROID_APP.get().context("no AndroidApp registered yet")
}

/// The JVM grows a local frame past this as needed; it only sizes the
/// initial allocation.
const LOCAL_FRAME_CAPACITY: i32 = 16;

/// Runs `f` with the current thread's JNI environment, attaching the thread
/// if it isn't yet. Local references made in `f` are freed when it returns,
/// and a Java exception it leaves pending is cleared and returned as the
/// error.
pub(crate) fn with_env<T>(
    android_app: &AndroidApp,
    f: impl FnOnce(&mut JNIEnv<'_>) -> Result<T>,
) -> Result<T> {
    let vm =
        unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast()) }.context("get the JavaVM")?;
    let mut env = vm
        .attach_current_thread()
        .context("attach the current thread to the JVM")?;
    if let Some(exception) = take_pending_exception(&mut env) {
        log::warn!("jni_env: cleared a Java exception left by an earlier call: {exception}");
    }
    let result = env.with_local_frame(LOCAL_FRAME_CAPACITY, |env| f(env));
    match take_pending_exception(&mut env) {
        Some(exception) => Err(match result {
            Err(error) => error.context(exception),
            Ok(_) => anyhow!(exception),
        }),
        None => result,
    }
}

/// [`with_env`] with the main Activity.
pub(crate) fn with_activity<T>(
    android_app: &AndroidApp,
    f: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> Result<T>,
) -> Result<T> {
    with_env(android_app, |env| {
        // android-activity holds a global reference to the Activity for as
        // long as the `AndroidApp` exists.
        let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr().cast()) };
        f(env, &activity)
    })
}

/// Calls a `void` method on a window's Activity: the main one for `None`, else
/// the extra window's. One whose Activity has gone is skipped.
pub(crate) fn call_window_method(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) -> Result<()> {
    with_activity(android_app, |env, main_activity| {
        match extra_window_id {
            Some(window_id) => {
                let Some(activity) = crate::multi_window::extra_activity_for(window_id) else {
                    return Ok(());
                };
                env.call_method(activity.as_obj(), method, signature, args)
            }
            None => env.call_method(main_activity, method, signature, args),
        }
        .with_context(|| format!("call {method} on window {extra_window_id:?}"))?;
        Ok(())
    })
}

/// [`call_window_method`] for every window that has an Activity. A failure on
/// one is logged and doesn't stop the others.
pub(crate) fn call_all_windows(
    android_app: &AndroidApp,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) {
    let extra_windows = crate::multi_window::extra_activity_ids();
    for window_id in std::iter::once(None).chain(extra_windows.into_iter().map(Some)) {
        call_window_method(android_app, window_id, method, signature, args).log_err();
    }
}

/// Calls a method on the main Activity that takes nothing and returns a
/// boolean.
pub(crate) fn call_main_activity_bool(method: &str) -> Result<bool> {
    with_activity(android_app()?, |env, activity| {
        Ok(env
            .call_method(activity, method, "()Z", &[])
            .with_context(|| format!("call MainActivity.{method}"))?
            .z()?)
    })
}

/// Clears a pending Java exception and returns its description.
fn take_pending_exception(env: &mut JNIEnv<'_>) -> Option<String> {
    if !env.exception_check().unwrap_or(false) {
        return None;
    }
    let exception = env
        .exception_occurred()
        .ok()
        .map(|exception| env.auto_local(exception));
    env.exception_describe().log_err();
    env.exception_clear().log_err();
    let description = exception
        .and_then(|exception| {
            env.call_method(&exception, "toString", "()Ljava/lang/String;", &[])
                .ok()
        })
        .and_then(|description| description.l().ok())
        .and_then(|description| {
            let description = env.auto_local(JString::from(description));
            env.get_string(&description).ok().map(String::from)
        });
    // `toString` itself can throw.
    env.exception_clear().log_err();
    Some(description.unwrap_or_else(|| "Java exception during a JNI call".to_string()))
}
