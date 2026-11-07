//! Android's clipboard, through `MainActivity`'s clipboard methods.
//!
//! `ClipboardItem` can carry images and structured entries. Text round-trips
//! here, and images can be pasted in, as Zed's agent does; copying an image out
//! isn't wired up, since nothing in Zed needs it. Zed reads the clipboard often,
//! such as to enable Paste in menus, and each read is a binder call, so a read
//! within [`CACHE_TTL`] of the last one reuses it. An image is only read once
//! per copy, for as long as `clipboardImageTimestamp` stays the same.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use anyhow::{Context as _, Result};
use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat};
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JString};

const CACHE_TTL: Duration = Duration::from_millis(50);

struct CachedImage {
    timestamp: i64,
    image: Option<Image>,
}

thread_local! {
    static READ_CACHE: RefCell<Option<(Instant, Option<ClipboardItem>)>> =
        const { RefCell::new(None) };
    static IMAGE_CACHE: RefCell<Option<CachedImage>> = const { RefCell::new(None) };
}

pub(crate) fn read(android_app: &AndroidApp) -> Option<ClipboardItem> {
    let cached = READ_CACHE.with_borrow(|cache| {
        cache
            .as_ref()
            .filter(|(read_at, _)| read_at.elapsed() < CACHE_TTL)
            .map(|(_, item)| item.clone())
    });
    cached.unwrap_or_else(|| {
        let text = read_text(android_app).unwrap_or_else(|error| {
            log::warn!("clipboard: read failed: {error:#}");
            None
        });
        let item = clipboard_item(android_app, text);
        READ_CACHE.set(Some((Instant::now(), item.clone())));
        item
    })
}

/// A clip with text is that text. An item with both would make vim's registers,
/// which only take a lone text entry, paste nothing.
fn clipboard_item(android_app: &AndroidApp, text: Option<String>) -> Option<ClipboardItem> {
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        IMAGE_CACHE.set(None);
        return Some(ClipboardItem::new_string(text));
    }
    let image = read_image(android_app).unwrap_or_else(|error| {
        log::warn!("clipboard: reading an image failed: {error:#}");
        None
    })?;
    Some(ClipboardItem::new_image(&image))
}

pub(crate) fn write(android_app: &AndroidApp, item: ClipboardItem) {
    let Some(text) = item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::String(string) => Some(string.text().to_owned()),
        _ => None,
    }) else {
        return;
    };
    let result = crate::jni_env::with_activity(android_app, |env, activity| {
        let text = env.new_string(&text)?;
        env.call_method(
            activity,
            "writeClipboardText",
            "(Ljava/lang/String;)V",
            &[(&text).into()],
        )?;
        Ok(())
    });
    if let Err(error) = result {
        log::warn!("clipboard: write failed: {error:#}");
    }
    // Android applies the write later, on its UI thread.
    READ_CACHE.set(Some((
        Instant::now(),
        Some(ClipboardItem::new_string(text)),
    )));
}

fn read_text(android_app: &AndroidApp) -> Result<Option<String>> {
    crate::jni_env::with_activity(android_app, |env, activity| {
        let text = env
            .call_method(activity, "readClipboardText", "()Ljava/lang/String;", &[])?
            .l()?;
        if text.is_null() {
            return Ok(None);
        }
        Ok(Some(env.get_string(&JString::from(text))?.into()))
    })
}

fn read_image(android_app: &AndroidApp) -> Result<Option<Image>> {
    crate::jni_env::with_activity(android_app, |env, activity| {
        let timestamp = env
            .call_method(activity, "clipboardImageTimestamp", "()J", &[])?
            .j()?;
        if timestamp == 0 {
            IMAGE_CACHE.set(None);
            return Ok(None);
        }
        let cached = IMAGE_CACHE.with_borrow(|cache| {
            cache
                .as_ref()
                .filter(|cached| cached.timestamp == timestamp)
                .map(|cached| cached.image.clone())
        });
        if let Some(image) = cached {
            return Ok(image);
        }
        // Cached even when it failed, so that a clip that can't be read doesn't
        // block drawing on every later read.
        let image = load_image(env, activity).unwrap_or_else(|error| {
            log::warn!("clipboard: couldn't load an image: {error:#}");
            None
        });
        IMAGE_CACHE.set(Some(CachedImage {
            timestamp,
            image: image.clone(),
        }));
        Ok(image)
    })
}

fn load_image(env: &mut JNIEnv<'_>, activity: &JObject<'_>) -> Result<Option<Image>> {
    let image = env
        .call_method(
            activity,
            "readClipboardImage",
            "()Lcom/zdroid/ClipboardImage;",
            &[],
        )?
        .l()?;
    if image.is_null() {
        return Ok(None);
    }
    let mime_type = env
        .get_field(&image, "mimeType", "Ljava/lang/String;")?
        .l()?;
    let mime_type: String = env.get_string(&JString::from(mime_type))?.into();
    let Some(format) = ImageFormat::from_mime_type(&mime_type) else {
        log::info!("clipboard: gpui has no image format for {mime_type}");
        return Ok(None);
    };
    let bytes = env.get_field(&image, "bytes", "[B")?.l()?;
    let bytes = env
        .convert_byte_array(JByteArray::from(bytes))
        .context("copy the image's bytes")?;
    Ok(Some(Image::from_bytes(format, bytes)))
}
