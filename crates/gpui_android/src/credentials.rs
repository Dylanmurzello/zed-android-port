//! Android Keystore-backed implementation of gpui's credential API.
//!
//! The Kotlin side (`SecureCredentialStore`) encrypts each entry with a
//! non-exportable AES-256-GCM Keystore key, binding the URL as associated
//! data, and keeps the ciphertext in app-private SharedPreferences. A read
//! returns `[u32 username length][username][password]`.

use android_activity::AndroidApp;
use anyhow::{Context as _, Result, anyhow};
use jni::{
    JNIEnv, JavaVM,
    objects::{JByteArray, JObject, JValue},
};
use util::ResultExt as _;

pub(crate) fn write(
    android_app: &AndroidApp,
    url: &str,
    username: &str,
    password: &[u8],
) -> Result<()> {
    with_activity(android_app, |env, activity| {
        let url = env.new_string(url)?;
        let username = env.new_string(username)?;
        let password = env.byte_array_from_slice(password)?;
        let stored = env
            .call_method(
                activity,
                "writeCredential",
                "(Ljava/lang/String;Ljava/lang/String;[B)Z",
                &[
                    JValue::Object(url.as_ref()),
                    JValue::Object(username.as_ref()),
                    JValue::Object(password.as_ref()),
                ],
            )?
            .z()?;
        if stored {
            Ok(())
        } else {
            Err(anyhow!("Android Keystore rejected credential write"))
        }
    })
}

pub(crate) fn read(android_app: &AndroidApp, url: &str) -> Result<Option<(String, Vec<u8>)>> {
    with_activity(android_app, |env, activity| {
        let url = env.new_string(url)?;
        let value = env
            .call_method(
                activity,
                "readCredential",
                "(Ljava/lang/String;)[B",
                &[JValue::Object(url.as_ref())],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }

        let bytes = env.convert_byte_array(JByteArray::from(value))?;
        let length_bytes: [u8; 4] = bytes
            .get(..4)
            .ok_or_else(|| anyhow!("invalid Android credential payload"))?
            .try_into()?;
        let username_len = u32::from_be_bytes(length_bytes) as usize;
        let username_end = 4usize
            .checked_add(username_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| anyhow!("invalid Android credential username length"))?;
        let username = String::from_utf8(bytes[4..username_end].to_vec())?;
        Ok(Some((username, bytes[username_end..].to_vec())))
    })
}

pub(crate) fn delete(android_app: &AndroidApp, url: &str) -> Result<()> {
    with_activity(android_app, |env, activity| {
        let url = env.new_string(url)?;
        let deleted = env
            .call_method(
                activity,
                "deleteCredential",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(url.as_ref())],
            )?
            .z()?;
        if deleted {
            Ok(())
        } else {
            Err(anyhow!("failed to delete Android credential"))
        }
    })
}

/// Runs `callback` with an attached JNI env and the activity, clearing any
/// pending Java exception before and after.
pub(crate) fn with_activity<T>(
    android_app: &AndroidApp,
    callback: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> Result<T>,
) -> Result<T> {
    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm
        .attach_current_thread()
        .context("attach current thread for Android credentials")?;
    // A Java exception left pending by an earlier JNI call on this thread
    // makes every following call fail, so clear it first; and clear any
    // exception our own call raises so it can't break the next caller.
    clear_pending_exception(&mut env);
    let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };
    let result = callback(&mut env, &activity);
    if clear_pending_exception(&mut env) {
        return Err(anyhow!("Java exception during Android credential JNI call"));
    }
    result
}

fn clear_pending_exception(env: &mut JNIEnv<'_>) -> bool {
    if !env.exception_check().unwrap_or(false) {
        return false;
    }
    env.exception_describe().log_err();
    env.exception_clear().log_err();
    true
}
