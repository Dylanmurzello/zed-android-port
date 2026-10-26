//! Bridge between gpui's `ClipboardItem` and Android's
//! `android.content.ClipboardManager`.
//!
//! gpui's stubbed implementations meant Cut/Copy never wrote anything and
//! Paste always saw an empty clipboard. We hop through JNI to call
//! `ClipboardManager.setPrimaryClip(ClipData.newPlainText(...))` and
//! `getPrimaryClip()` so editor copy/cut/paste actions interoperate with
//! the rest of Android.
//!
//! Plain text only — `ClipboardItem` can carry images and structured
//! entries but our editor only round-trips text through it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use anyhow::Result;
use gpui::ClipboardItem;
use jni::{JavaVM, objects::{JObject, JValue}};

const SERVICE_CLIPBOARD: &str = "clipboard";

// gpui calls `cx.read_from_clipboard()` opportunistically during render —
// e.g., to gate the Paste menu item's enabled state. Each call is a JNI
// round-trip into `ClipboardManager.getPrimaryClip()`, which on Android
// goes through Binder IPC to system_server. At 60fps this allocates Java
// stack frames faster than the JVM can recycle them, eventually
// overflowing the 988KB android_main thread stack and aborting the
// process via CheckJNI's "pending exception" guard.
//
// 50ms cache TTL is short enough that pasted content from another app
// shows up "instantly" by human standards while keeping the JNI rate
// at <20 calls/sec instead of 60+/sec.
static READ_CACHE: Mutex<Option<(Instant, Option<String>)>> =
    Mutex::new(None);

// Belt-and-braces against re-entry: if read() is somehow called from a
// callback fired during another read (shouldn't happen but defends
// against gpui evolution), the second caller bails instead of stacking.
static READ_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static WRITE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

const CACHE_TTL: Duration = Duration::from_millis(50);

pub(crate) fn read(android_app: &AndroidApp) -> Option<ClipboardItem> {
    if let Some(cached) = read_cached() {
        return cached.map(ClipboardItem::new_string);
    }
    if READ_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        log::debug!("clipboard: read re-entered; returning None");
        return None;
    }
    let result = match read_inner(android_app) {
        Ok(text) => text,
        Err(err) => {
            log::warn!("clipboard: read failed: {err:#}");
            None
        }
    };
    // Belt-and-braces: drain any pending Java exception before returning
    // control. CheckJNI SIGABRTs the whole app on the next foreign JNI
    // call (cursor, saf, storage, …) if state is left behind.
    drain_pending_exception(android_app);
    READ_IN_FLIGHT.store(false, Ordering::Release);

    if let Ok(mut guard) = READ_CACHE.lock() {
        *guard = Some((Instant::now(), result.clone()));
    }

    result.map(ClipboardItem::new_string)
}

fn read_cached() -> Option<Option<String>> {
    let guard = READ_CACHE.lock().ok()?;
    let (when, value) = guard.as_ref()?;
    (when.elapsed() < CACHE_TTL).then(|| value.clone())
}

pub(crate) fn write(android_app: &AndroidApp, item: ClipboardItem) {
    let Some(text) = primary_text(&item) else {
        log::info!("clipboard: write skipped — no text in ClipboardItem");
        return;
    };
    log::info!("clipboard: write called with {} bytes", text.len());
    if WRITE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        log::debug!("clipboard: write re-entered; skipping");
        return;
    }
    // The JNI chain (attach_current_thread → getSystemService →
    // ClipData.newPlainText → setPrimaryClip) eats a few hundred KB of
    // Java stack, and the gpui render/dispatch thread that calls us
    // already runs at ~70-90% of the 988KB android_main stack.
    // Calling JNI from here trips StackOverflowError on the JVM side
    // (mid-printStackTrace, no less — the linker can't even tell us
    // what failed). Same class of bug as the read-path 60fps cascade.
    // See `docs/workarounds/jvm-clipboard-stack-overflow.md`.
    //
    // Fix: fire-and-forget on a fresh thread with 2MB stack. AndroidApp
    // doesn't expose a Clone; we extract the raw JavaVM* and Activity*
    // (process-global, stable for the app's lifetime) and rebuild
    // jni::JavaVM on the worker.
    let vm_ptr = android_app.vm_as_ptr() as usize;
    let activity_ptr = android_app.activity_as_ptr() as usize;

    let spawned = std::thread::Builder::new()
        .name("zed-clipboard-write".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let outcome = write_on_worker(vm_ptr, activity_ptr, &text);
            WRITE_IN_FLIGHT.store(false, Ordering::Release);
            if let Err(err) = outcome {
                log::warn!("clipboard: write failed: {err:#}");
                return;
            }
            // Invalidate the read cache — an external observer of our
            // clipboard would see this fresh write, so subsequent reads
            // should round-trip.
            if let Ok(mut guard) = READ_CACHE.lock() {
                *guard = None;
            }
        });

    // If thread spawn itself fails, release the latch so we don't get
    // wedged in an infinite "in-flight" state.
    if spawned.is_err() {
        WRITE_IN_FLIGHT.store(false, Ordering::Release);
        log::warn!("clipboard: write thread spawn failed");
    }
}

fn write_on_worker(vm_ptr: usize, activity_ptr: usize, text: &str) -> Result<()> {
    let vm = unsafe { JavaVM::from_raw(vm_ptr as *mut _)? };
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { JObject::from_raw(activity_ptr as _) };

    let _ = clear_pending_exception(&mut env);

    let manager = clipboard_manager(&mut env, &activity)?;
    let label = env.new_string("")?;
    let payload = env.new_string(text)?;
    let clip_data_class = env.find_class("android/content/ClipData")?;
    let clip = env
        .call_static_method(
            &clip_data_class,
            "newPlainText",
            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
            &[(&label).into(), (&payload).into()],
        )?
        .l()?;
    env.call_method(
        &manager,
        "setPrimaryClip",
        "(Landroid/content/ClipData;)V",
        &[(&clip).into()],
    )?;
    let _ = clear_pending_exception(&mut env);
    Ok(())
}

fn primary_text(item: &ClipboardItem) -> Option<String> {
    item.entries().iter().find_map(|entry| match entry {
        gpui::ClipboardEntry::String(s) => Some(s.text().to_owned()),
        _ => None,
    })
}

fn read_inner(android_app: &AndroidApp) -> Result<Option<String>> {
    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };

    // Defensively clear any pending exception before doing JNI work.
    // jni-rs's `?` does clear JavaException for known-failed call_methods,
    // but error paths that don't reach a Java call (WrongJValueType,
    // attach failures, etc.) can still leave state. CheckJNI SIGABRTs
    // on the next call if anything is pending — we've crashed twice on
    // exactly this with Pending StackOverflowError after a clipboard
    // failure on Android 14+.
    let _ = clear_pending_exception(&mut env);

    let manager = clipboard_manager(&mut env, &activity)?;
    let has_clip = env
        .call_method(&manager, "hasPrimaryClip", "()Z", &[])?
        .z()?;
    if !has_clip {
        return Ok(None);
    }
    let clip = env
        .call_method(
            &manager,
            "getPrimaryClip",
            "()Landroid/content/ClipData;",
            &[],
        )?
        .l()?;
    if clip.is_null() {
        return Ok(None);
    }
    let count = env.call_method(&clip, "getItemCount", "()I", &[])?.i()?;
    if count <= 0 {
        return Ok(None);
    }
    let item = env
        .call_method(
            &clip,
            "getItemAt",
            "(I)Landroid/content/ClipData$Item;",
            &[JValue::Int(0)],
        )?
        .l()?;
    if item.is_null() {
        return Ok(None);
    }
    let text = env
        .call_method(&item, "getText", "()Ljava/lang/CharSequence;", &[])?
        .l()?;
    if text.is_null() {
        return Ok(None);
    }
    let text_str = env
        .call_method(&text, "toString", "()Ljava/lang/String;", &[])?
        .l()?;
    let s: String = env.get_string(&text_str.into())?.into();
    Ok(Some(s))
}

/// Drain any Java exception currently flagged on this thread. Logs the
/// exception class+message so we can diagnose, then clears so subsequent
/// JNI calls don't hit CheckJNI's pending-exception abort.
fn clear_pending_exception(env: &mut jni::JNIEnv) -> Result<()> {
    if !env.exception_check()? {
        return Ok(());
    }
    // exception_describe writes to logcat via Android's default uncaught
    // handler — useful breadcrumb for which call generated the exception.
    let _ = env.exception_describe();
    env.exception_clear()?;
    log::warn!("clipboard: cleared pending Java exception");
    Ok(())
}

/// Public-level safety net: re-attach to the JVM and drain any pending
/// exception. Cheap when there's nothing to clear (just an exception_check
/// call). Called from `read`/`write` AFTER the inner work returns to make
/// sure no exception leaks into other JNI paths (cursor, saf, storage).
fn drain_pending_exception(android_app: &AndroidApp) {
    let Ok(vm) = (unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast()) }) else {
        return;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    let _ = clear_pending_exception(&mut env);
}

fn clipboard_manager<'a>(
    env: &mut jni::JNIEnv<'a>,
    activity: &JObject<'a>,
) -> Result<JObject<'a>> {
    let service_name = env.new_string(SERVICE_CLIPBOARD)?;
    let manager = env
        .call_method(
            activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[(&service_name).into()],
        )?
        .l()?;
    if manager.is_null() {
        anyhow::bail!("getSystemService(CLIPBOARD_SERVICE) returned null");
    }
    Ok(manager)
}

