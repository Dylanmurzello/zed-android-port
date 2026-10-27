//! JNI calls into the app's Java code.
//!
//! GameActivity attaches the gpui thread to the JVM for the life of the app,
//! so what a JNI call leaves behind isn't cleaned up by a detach: local
//! references pile up in the thread's reference table, and a pending Java
//! exception makes every later JNI call on the thread fail (CheckJNI aborts
//! the process). Every call goes through [`with_env`] or [`with_activity`],
//! which free the call's local references and clear its exceptions.

use android_activity::AndroidApp;
use anyhow::{Context as _, Result, anyhow};
use jni::{
    JNIEnv, JavaVM,
    objects::{JObject, JString},
};
use util::ResultExt as _;

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
