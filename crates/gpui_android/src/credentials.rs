//! Android Keystore-backed implementation of gpui's credential API.
//!
//! The Kotlin side (`SecureCredentialStore`) encrypts each entry with a
//! non-exportable AES-256-GCM Keystore key, binding the URL as associated
//! data, and keeps the ciphertext in app-private SharedPreferences. A read
//! returns `[u32 username length][username][password]`.

use android_activity::AndroidApp;
use anyhow::{Result, anyhow};
use jni::objects::{JByteArray, JValue};

use crate::jni_env::with_activity;

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
