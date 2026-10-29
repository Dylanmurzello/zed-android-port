//! Multi-window bridge: each `cx.open_window` beyond the GameActivity-owned
//! primary spawns an `ExtraWindowActivity` (separate AppCompatActivity), and
//! its `SurfaceView`'s native window backs a fresh `VkSurfaceKHR` on the Rust
//! side. On freeform-windowing devices (DeX, Pixel desktop windowing,
//! Android 16 Desktop Mode, ChromeOS) each Activity becomes a real OS-chromed
//! freeform window with close X / drag bar / resize handles. On phones
//! (non-freeform) each Activity lands in its own Recents task; usable but no
//! chrome.
//!
//! Threading: `create_extra_window_blocking` runs on the gpui main (game)
//! thread inside `AndroidPlatform::open_window`. It posts the Activity launch
//! via JNI `startActivity`, then blocks the game thread on a `oneshot` until
//! the Activity's first `surfaceCreated` callback fires. A 500ms hard timeout
//! guards against ANR-class freezes if Android is slow to launch the Activity.
//!
//! Touch + ongoing-lifecycle events arrive on the UI thread (via
//! `NativeBridge` JNI hooks) and are forwarded to the game thread through an
//! `mpsc` channel that `AndroidPlatform::run` drains each iteration.
//!
//! ## Activity ref tracking
//!
//! `finishAndRemoveTask` must target a specific `ExtraWindowActivity`
//! instance. We track them in a process-global registry keyed by `window_id`:
//!
//! - `nativeOnExtraActivityCreated` (called from `ExtraWindowActivity.onCreate`)
//!   wraps the activity in a `GlobalRef` and inserts.
//! - `finish_extra_activity` reads from the registry and calls
//!   `finishAndRemoveTask` on the stored ref.
//! - `nativeOnExtraActivityDestroyed` removes the ref (drops the `GlobalRef`)
//!   AND posts `OsClosed` so the gpui-side window can be reaped.
//!
//! **Thread constraint:** map mutations (insert + remove) must happen from
//! the gpui main thread only. The `jni` crate calls `DeleteGlobalRef` via
//! whatever `JNIEnv` the dropping thread can attach via
//! `JavaVM::attach_current_thread`. Drops on a non-attachable thread (e.g. a
//! tokio worker after its `AttachGuard` has been released) silently leak.
//! The gpui main thread stays attached for the App's lifetime; doing all map
//! work from the drain handler is the safe choice.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use android_activity::AndroidApp;
use futures::channel::{mpsc, oneshot};
use jni::JavaVM;
use jni::objects::{GlobalRef, JFloatArray, JIntArray, JObject, JValue};
use ndk::native_window::NativeWindow;

/// Hard cap on how long `create_extra_window_blocking` will wait for the
/// Activity's `surfaceCreated` callback. Cold Activity start on a Snapdragon
/// 8 Gen 2 in DeX desktop windowing is ~530-700ms; the budget covers that
/// plus headroom. If exceeded we return Err and gpui surfaces the failure to
/// the caller (UX: "couldn't open, try again") instead of freezing the
/// Workspace forever. Yes 2s is long; warm reopens are sub-100ms because
/// the Activity stays alive in Recents.
const ACTIVITY_LAUNCH_TIMEOUT: Duration = Duration::from_millis(4000);

/// Events from the JNI side (UI thread) that the gpui main thread needs to
/// process. Sent via `EVENT_TX`, drained in the platform's main loop.
pub(crate) enum ExtraWindowEvent {
    /// `surfaceChanged` for an existing extra window.
    Resized {
        window_id: u64,
        width: u32,
        height: u32,
    },
    /// `surfaceDestroyed` — Vulkan surface should be torn down.
    SurfaceDestroyed { window_id: u64 },
    /// OS-initiated Activity destruction (user clicked the chrome X, or
    /// system swiped the task off Recents). The drain handler must invoke
    /// the gpui-registered `on_close` callback to drive `remove_window`,
    /// then reap our maps. The `GlobalRef` for the Activity has already been
    /// dropped from `EXTRA_ACTIVITY_REFS` by the time this event fires.
    OsClosed { window_id: u64 },
    /// Touch / pointer event on the extra `SurfaceView`. Raw fields from the
    /// Java `MotionEvent`; the platform translates them on the game thread to
    /// avoid touching the JNI env from a non-UI thread.
    Motion {
        window_id: u64,
        action_masked: i32,
        action_index: i32,
        meta_state: i32,
        button_state: i32,
        event_time_millis: i64,
        vscroll: f32,
        hscroll: f32,
        positions: Vec<(f32, f32, i32)>,
    },
    /// Hardware key event on the extra window. Raw fields from the Java
    /// `KeyEvent`; same off-thread translation policy as `Motion`. Without
    /// this routing the Editor in any extra window (Settings is the
    /// canonical case) never receives keystrokes — `set_input_handler`
    /// runs on focus, but no `PlatformInput::KeyDown` is ever fed in, so
    /// the search bar and any other Editor look frozen.
    Key {
        window_id: u64,
        action: i32,
        keycode: u32,
        meta_state: u32,
        repeat_count: i32,
    },
}

/// Senders for the first `surfaceCreated` of each extra window. The receiver
/// is held by the game thread inside `create_extra_window_blocking` and is
/// dropped after the surface arrives.
static SURFACE_CREATED_TX: Mutex<Option<HashMap<u64, oneshot::Sender<NativeWindow>>>> =
    Mutex::new(None);

/// Per-`window_id` `GlobalRef` to the live `ExtraWindowActivity` instance.
/// Inserted in `nativeOnExtraActivityCreated`; removed in
/// `nativeOnExtraActivityDestroyed` or via `finish_extra_activity`.
/// See module docs for the strict thread constraint on mutations.
static EXTRA_ACTIVITY_REFS: Mutex<Option<HashMap<u64, GlobalRef>>> = Mutex::new(None);

/// Window IDs that the gpui side has fully attached and registered. Inserted
/// in `AndroidPlatform::open_extra_window` AFTER `attach_surface` succeeds and
/// the window enters `extra_windows`; removed in the `OsClosed` drain handler.
/// Read by `ExtraWindowActivity.onCreate` via `nativeIsExtraWindowKnown` to
/// detect process-resurrection: when Android kills+restarts the process and
/// brings ExtraWindowActivity back from Recents, this set is empty for the
/// resurrected windowId, so the Activity finishes itself instead of running
/// uselessly without a Rust counterpart.
///
/// Distinct from `EXTRA_ACTIVITY_REFS` — that map is JNI-side state set
/// inside `nativeOnExtraActivityCreated`, which fires BEFORE this check, so
/// it can't be the source of truth.
static REGISTERED_WINDOWS: Mutex<Option<std::collections::HashSet<u64>>> = Mutex::new(None);

/// Sender side of the ongoing event channel. Replaced each time
/// `AndroidPlatform::new` calls [`init_event_channel`]. Cloned per JNI
/// callback for thread-safe sends.
///
/// We use a Mutex (not a OnceLock) because Android may recreate the
/// hosting Activity within the same OS process — e.g. after a
/// configuration change, an OOM-recovery kill that didn't actually
/// free the .so, or DeX windowing-mode flips. Each Activity recreation
/// re-enters `android_main`, which constructs a fresh AndroidPlatform
/// and expects a fresh receiver. Panicking on the second init is the
/// "activity-recreation-idempotency" trap our docs warn about.
static EVENT_TX: Mutex<Option<mpsc::UnboundedSender<ExtraWindowEvent>>> = Mutex::new(None);

/// Set up the ongoing event channel. Safe to call multiple times: each
/// invocation creates a fresh sender/receiver pair. Any previous sender
/// is dropped (which closes its receiver in the prior platform instance,
/// but that platform is being torn down anyway). Returns the new
/// receiver, which the current platform drains each iteration of its
/// event loop.
pub(crate) fn init_event_channel() -> mpsc::UnboundedReceiver<ExtraWindowEvent> {
    let (tx, rx) = mpsc::unbounded();
    *EVENT_TX.lock().unwrap() = Some(tx);
    rx
}

fn pending_table()
-> std::sync::MutexGuard<'static, Option<HashMap<u64, oneshot::Sender<NativeWindow>>>> {
    SURFACE_CREATED_TX.lock().unwrap()
}

fn refs_table() -> std::sync::MutexGuard<'static, Option<HashMap<u64, GlobalRef>>> {
    EXTRA_ACTIVITY_REFS.lock().unwrap()
}

/// Clone the `GlobalRef` for the `ExtraWindowActivity` with this
/// `window_id`. Returns `None` if the Activity isn't registered
/// (already destroyed, or never created). Callers that need to call
/// Kotlin methods on a specific extra Activity (cursor sprite for
/// trackpad mode, per-window IME mode, etc.) go through this.
pub(crate) fn extra_activity_for(window_id: u64) -> Option<GlobalRef> {
    refs_table().as_ref().and_then(|m| m.get(&window_id)).cloned()
}

/// Snapshot every registered extra-window id so callers can fan a
/// JNI operation out to all spawned `ExtraWindowActivity` instances
/// without holding the global mutex across the inner JNI calls
/// (which would deadlock if the callback re-entered the registry).
pub(crate) fn extra_activity_ids() -> Vec<u64> {
    refs_table()
        .as_ref()
        .map(|m| m.keys().copied().collect())
        .unwrap_or_default()
}

fn registered_set()
-> std::sync::MutexGuard<'static, Option<std::collections::HashSet<u64>>> {
    REGISTERED_WINDOWS.lock().unwrap()
}

/// Called by `AndroidPlatform::open_extra_window` after `attach_surface`
/// succeeds and the window enters `extra_windows`. Marks the windowId as
/// "live in this Rust process" so `nativeIsExtraWindowKnown` can detect a
/// later resurrection.
pub(crate) fn mark_window_registered(window_id: u64) {
    let mut set = registered_set();
    set.get_or_insert_with(std::collections::HashSet::new)
        .insert(window_id);
}

/// Called by the `OsClosed` drain handler after `extra_windows.remove`.
/// Keeps `REGISTERED_WINDOWS` in sync so a re-opened window with the same id
/// (rare but possible if gpui reuses a `WindowId` slot) doesn't see a stale
/// entry.
pub(crate) fn unmark_window_registered(window_id: u64) {
    if let Some(set) = registered_set().as_mut() {
        set.remove(&window_id);
    }
}

/// Optional launch bounds (in device pixels) the OS uses as the initial
/// freeform window rect. `None` lets the system pick — typically a centered
/// default. Passed straight through to `ActivityOptions.setLaunchBounds`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LaunchBounds {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
}

/// Block the game thread until the `ExtraWindowActivity` for `window_id` has
/// fired its first `surfaceCreated`, returning the wrapped `NativeWindow`.
/// On timeout (`ACTIVITY_LAUNCH_TIMEOUT`) returns Err. The caller is
/// responsible for subsequently creating an `AndroidWindow` and registering
/// it with [`crate::platform::AndroidPlatform`] so future
/// `ExtraWindowEvent`s for this id route correctly.
pub(crate) fn create_extra_window_blocking(
    android_app: &AndroidApp,
    window_id: u64,
    bounds: Option<LaunchBounds>,
) -> Result<NativeWindow> {
    let (tx, rx) = oneshot::channel();
    {
        let mut table = pending_table();
        let map = table.get_or_insert_with(HashMap::new);
        if map.contains_key(&window_id) {
            bail!("extra window {window_id} already pending");
        }
        map.insert(window_id, tx);
    }
    if let Err(err) = launch_extra_activity(android_app, window_id, bounds) {
        pending_table().as_mut().and_then(|m| m.remove(&window_id));
        return Err(err.context("ExtraWindowActivity launch failed"));
    }
    // `oneshot::Receiver` is a `Future`. Drive it to completion synchronously
    // with a hard timeout so a stalled Activity launch can't lock the game
    // thread forever (cold Activity start is normally 200-400ms; cap at
    // 500ms — see `ACTIVITY_LAUNCH_TIMEOUT`).
    //
    // `try_recv` returns: `Ok(Some(v))` value received; `Ok(None)` not sent
    // yet, channel still open; `Err(Canceled)` sender dropped without sending.
    let deadline = std::time::Instant::now() + ACTIVITY_LAUNCH_TIMEOUT;
    let mut rx = rx;
    loop {
        match rx.try_recv() {
            Ok(Some(native_window)) => return Ok(native_window),
            Ok(None) => {
                // Not ready yet — fall through to deadline check + sleep.
            }
            Err(_) => {
                pending_table().as_mut().and_then(|m| m.remove(&window_id));
                bail!("extra surface creation channel dropped (sender canceled)");
            }
        }
        if std::time::Instant::now() >= deadline {
            pending_table().as_mut().and_then(|m| m.remove(&window_id));
            bail!(
                "ExtraWindowActivity startup exceeded {}ms",
                ACTIVITY_LAUNCH_TIMEOUT.as_millis()
            );
        }
        // Yield briefly. We can't park the future cleanly without an executor,
        // so a short sleep is the simplest correct choice.
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Bring the `ExtraWindowActivity` for `window_id` to the foreground.
/// Routes to `ActivityManager.AppTask.moveToFront()` for the Activity's
/// own task — that's the official self-only API that doesn't need the
/// `REORDER_TASKS` permission. Best-effort; silently no-ops if the
/// windowId isn't registered (Activity already destroyed) or the task
/// isn't found in the app's own task list (system reaped it).
///
/// Without this, gpui's `Window::activate_window()` is a no-op on Android.
/// settings_ui's existing-window dedup at `settings_ui.rs:622` calls
/// `window.activate_window()` after finding an open `SettingsWindow`,
/// expecting the OS to surface that window — on Android we have to
/// implement that surfacing ourselves.
pub(crate) fn activate_extra_activity(android_app: &AndroidApp, window_id: u64) {
    let result = (|| -> Result<()> {
        let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
        let mut env = vm.attach_current_thread()?;
        let activity_ref = match refs_table().as_ref().and_then(|m| m.get(&window_id)).cloned() {
            Some(ar) => ar,
            None => return Ok(()),
        };

        // taskId of the ExtraWindowActivity. With `documentLaunchMode="always"`
        // each Activity is in its own task so taskId uniquely identifies it.
        let task_id = env
            .call_method(activity_ref.as_obj(), "getTaskId", "()I", &[])?
            .i()?;

        // Get ActivityManager — `Activity.getSystemService(Context.ACTIVITY_SERVICE)`
        let activity_service = env.new_string("activity")?;
        let activity_manager = env
            .call_method(
                activity_ref.as_obj(),
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&activity_service)],
            )?
            .l()?;

        // List<AppTask> tasks = activityManager.getAppTasks();
        let app_tasks = env
            .call_method(&activity_manager, "getAppTasks", "()Ljava/util/List;", &[])?
            .l()?;
        let size = env.call_method(&app_tasks, "size", "()I", &[])?.i()?;
        for i in 0..size {
            let app_task = env
                .call_method(
                    &app_tasks,
                    "get",
                    "(I)Ljava/lang/Object;",
                    &[JValue::Int(i)],
                )?
                .l()?;
            // ActivityManager.AppTask.getTaskInfo() returns ActivityManager.RecentTaskInfo,
            // which extends TaskInfo. The taskId field is `id` on RecentTaskInfo
            // (deprecated since API 29, still populated) and `taskId` on
            // TaskInfo (API 29+). Try the modern field first.
            let task_info = env
                .call_method(
                    &app_task,
                    "getTaskInfo",
                    "()Landroid/app/ActivityManager$RecentTaskInfo;",
                    &[],
                )?
                .l()?;
            let id = match env.get_field(&task_info, "taskId", "I") {
                Ok(v) => v.i()?,
                Err(_) => {
                    if env.exception_check().unwrap_or(false) {
                        let _ = env.exception_clear();
                    }
                    env.get_field(&task_info, "id", "I")?.i()?
                }
            };
            if id == task_id {
                env.call_method(&app_task, "moveToFront", "()V", &[])?;
                break;
            }
        }

        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        Ok(())
    })();
    if let Err(err) = result {
        log::warn!("multi_window: activate_extra_activity({window_id}): {err:#}");
    }
}

/// Set the title on the `ExtraWindowActivity` for `window_id`. Shows up in
/// the OS chrome's drag bar (in freeform/desktop windowing) and in Recents.
/// Best-effort — silently no-ops if the windowId isn't registered (e.g.
/// gpui called set_title before the Activity finished launching, or after
/// it closed).
pub(crate) fn set_extra_activity_title(android_app: &AndroidApp, window_id: u64, title: &str) {
    let result = (|| -> Result<()> {
        let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
        let mut env = vm.attach_current_thread()?;
        let activity_ref = match refs_table().as_ref().and_then(|m| m.get(&window_id)).cloned() {
            Some(ar) => ar,
            None => return Ok(()),
        };
        let title_jstr = env.new_string(title)?;
        env.call_method(
            activity_ref.as_obj(),
            "setTitle",
            "(Ljava/lang/CharSequence;)V",
            &[JValue::Object(&title_jstr)],
        )?;
        // CharSequence subtype check passes for String — JVM dispatches to
        // setTitle(CharSequence) which delegates to the Activity's window.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        Ok(())
    })();
    if let Err(err) = result {
        log::warn!("multi_window: set_extra_activity_title({window_id}): {err:#}");
    }
}

/// Tell the JVM side to finish the `ExtraWindowActivity` for `window_id`,
/// removing it from screen and Recents. Idempotent — if the Activity has
/// already destroyed (e.g. user clicked the OS chrome X), the registry entry
/// is gone and this is a no-op.
pub(crate) fn finish_extra_activity(android_app: &AndroidApp, window_id: u64) {
    let activity = match refs_table().as_mut().and_then(|m| m.remove(&window_id)) {
        Some(activity) => activity,
        None => return, // already destroyed or never registered
    };
    if let Err(err) = call_finish_and_remove_task(android_app, &activity) {
        log::warn!("multi_window: finish_extra_activity({window_id}): {err:#}");
    }
    // GlobalRef drops here on the game thread (see thread-constraint comment
    // at module top), which calls DeleteGlobalRef via the JVM properly.
    drop(activity);
}

fn launch_extra_activity(
    android_app: &AndroidApp,
    window_id: u64,
    bounds: Option<LaunchBounds>,
) -> Result<()> {
    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm.attach_current_thread()?;
    let result = launch_extra_activity_inner(&mut env, android_app, window_id, bounds);
    // The jni crate's `?`-propagation surfaces a Rust error but leaves the
    // JNI env with a *pending* Java exception. Subsequent JNI calls (e.g.
    // any logger that touches JNI on cleanup) trip
    // "JNI GetObjectClass called with pending exception" → process abort.
    // Always clear before returning.
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    result
}

fn launch_extra_activity_inner(
    env: &mut jni::AttachGuard<'_>,
    android_app: &AndroidApp,
    window_id: u64,
    bounds: Option<LaunchBounds>,
) -> Result<()> {
    let main_activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };

    // Resolve `ExtraWindowActivity` via MainActivity's ClassLoader. Android
    // splits classloaders per app — the system classloader doesn't see app
    // classes, so `Class.forName(name)` fails with ClassNotFoundException.
    // The standard pattern is to grab the Activity's ClassLoader (which
    // knows about /data/app/<pkg>/base.apk's classes) and use that.
    let main_class = env.get_object_class(&main_activity)?;
    let class_loader = env
        .call_method(&main_class, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let class_name = env.new_string("com.zdroid.ExtraWindowActivity")?;
    let extra_class = env
        .call_method(
            &class_loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&class_name)],
        )?
        .l()?;

    let intent_class = env.find_class("android/content/Intent")?;
    let intent = env.new_object(
        &intent_class,
        "(Landroid/content/Context;Ljava/lang/Class;)V",
        &[JValue::Object(&main_activity), JValue::Object(&extra_class)],
    )?;

    let extra_key = env.new_string("com.zdroid.window_id")?;
    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;J)Landroid/content/Intent;",
        &[JValue::Object(&extra_key), JValue::Long(window_id as i64)],
    )?;

    // `documentLaunchMode="always"` on the manifest already implies
    // FLAG_ACTIVITY_NEW_DOCUMENT | FLAG_ACTIVITY_MULTIPLE_TASK, so we don't
    // set them here — setting them additionally was causing MainActivity
    // to be backgrounded under DeX freeform windowing.
    if let Some(rect) = bounds {
        // Build ActivityOptions.makeBasic().setLaunchBounds(Rect) and pass
        // its Bundle to startActivity. Lets us request an initial freeform
        // window rect (size + position) instead of letting the OS pick.
        let rect_class = env.find_class("android/graphics/Rect")?;
        let rect_obj = env.new_object(
            &rect_class,
            "(IIII)V",
            &[
                JValue::Int(rect.left),
                JValue::Int(rect.top),
                JValue::Int(rect.right),
                JValue::Int(rect.bottom),
            ],
        )?;
        let activity_options_class = env.find_class("android/app/ActivityOptions")?;
        let opts = env
            .call_static_method(
                &activity_options_class,
                "makeBasic",
                "()Landroid/app/ActivityOptions;",
                &[],
            )?
            .l()?;
        env.call_method(
            &opts,
            "setLaunchBounds",
            "(Landroid/graphics/Rect;)Landroid/app/ActivityOptions;",
            &[JValue::Object(&rect_obj)],
        )?;
        let bundle = env
            .call_method(&opts, "toBundle", "()Landroid/os/Bundle;", &[])?
            .l()?;
        env.call_method(
            &main_activity,
            "startActivity",
            "(Landroid/content/Intent;Landroid/os/Bundle;)V",
            &[JValue::Object(&intent), JValue::Object(&bundle)],
        )?;
    } else {
        env.call_method(
            &main_activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[JValue::Object(&intent)],
        )?;
    }

    Ok(())
}

fn call_finish_and_remove_task(_android_app: &AndroidApp, activity: &GlobalRef) -> Result<()> {
    let vm = unsafe { JavaVM::from_raw(_android_app.vm_as_ptr().cast())? };
    let mut env = vm.attach_current_thread()?;
    env.call_method(activity.as_obj(), "finishAndRemoveTask", "()V", &[])?;
    Ok(())
}

fn dispatch_event(event: ExtraWindowEvent) {
    let guard = EVENT_TX.lock().unwrap();
    let Some(tx) = guard.as_ref() else {
        log::warn!("multi_window: event arrived before init_event_channel");
        return;
    };
    if let Err(err) = tx.unbounded_send(event) {
        log::warn!("multi_window: dispatch_event: {err:#}");
    }
}

/// Process-death recovery probe. Called by `ExtraWindowActivity.onCreate`
/// before any other JNI work. Returns true if the gpui-side has a live
/// AndroidWindow registered for this `windowId` — i.e. this Activity was
/// launched in the current Rust process and gpui knows about it. Returns
/// false on resurrection (the OS brought the Activity back from Recents
/// but the process was killed and restarted, so gpui has no record).
/// Activity uses the result to decide whether to proceed or `finish()`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeIsExtraWindowKnown<
    'local,
>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) -> jni::sys::jboolean {
    let window_id = window_id as u64;
    let known = registered_set()
        .as_ref()
        .map(|s| s.contains(&window_id))
        .unwrap_or(false);
    log::info!("multi_window: nativeIsExtraWindowKnown windowId={window_id} → {known}");
    jni::sys::jboolean::from(known)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraActivityCreated<
    'local,
>(
    env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    activity: JObject<'local>,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraActivityCreated windowId={window_id}");
    // Note on threading: this fires on the JVM thread that called the
    // external fn — typically the Android UI thread. We need a JNIEnv to
    // create the GlobalRef, so we use the supplied `env` here. The actual
    // map mutation (insert) is fine on this thread because GlobalRef
    // creation is symmetric — only `DeleteGlobalRef` (i.e. drop) must be
    // attentive to thread attachment, and we drop in `finish_extra_activity`
    // on the gpui main thread.
    let global_ref = match env.new_global_ref(activity) {
        Ok(r) => r,
        Err(err) => {
            log::error!(
                "multi_window: failed to create GlobalRef for activity windowId={window_id}: {err:#}"
            );
            return;
        }
    };
    let mut table = refs_table();
    let map = table.get_or_insert_with(HashMap::new);
    map.insert(window_id, global_ref.clone());
    drop(table); // release the mutex before the JNI call below

    // If trackpad mode is currently on, push the active state to
    // this newly-created activity so its cursor sprite appears the
    // moment its surface is ready. Without this, opening an extra
    // window while trackpad mode is on leaves it without a cursor
    // until the user toggles trackpad mode off-and-on again.
    let mut env = env;
    if crate::ime::trackpad_mode_enabled()
        && let Err(err) = env.call_method(
            global_ref.as_obj(),
            "setTrackpadModeActive",
            "(Z)V",
            &[true.into()],
        )
    {
        log::warn!(
            "multi_window: failed to push trackpad-mode state to new extra activity {window_id}: {err:#}"
        );
    }

    // Also push the current cursor style. The `set_pointer_icon`
    // cache short-circuits the per-event JNI fan-out when gpui's
    // cursor style hasn't changed since the last push, so a
    // newly-spawned activity could otherwise sit at its default
    // arrow sprite while the rest of the app is on (say) IBeam
    // because the cursor was over a text input when the window
    // was opened.
    if let Some(icon_type) = crate::cursor::last_pushed_icon_type()
        && let Err(err) = env.call_method(
            global_ref.as_obj(),
            "setCapturedCursorStyle",
            "(I)V",
            &[icon_type.into()],
        )
    {
        log::warn!(
            "multi_window: failed to push cursor style to new extra activity {window_id}: {err:#}"
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraActivityDestroyed<
    'local,
>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraActivityDestroyed windowId={window_id}");
    // Drop the GlobalRef from the JVM thread (which is attached). The
    // OsClosed event then triggers gpui-side teardown on the game thread.
    let _ = refs_table().as_mut().and_then(|m| m.remove(&window_id));
    dispatch_event(ExtraWindowEvent::OsClosed { window_id });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraSurfaceCreated<
    'local,
>(
    env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    surface: JObject<'local>,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraSurfaceCreated windowId={window_id}");
    let native_window = unsafe { NativeWindow::from_surface(env.get_raw(), surface.as_raw()) };
    let Some(native_window) = native_window else {
        log::error!(
            "multi_window: ANativeWindow_fromSurface returned null for windowId={window_id}"
        );
        return;
    };
    let pending = pending_table().as_mut().and_then(|m| m.remove(&window_id));
    if let Some(tx) = pending {
        let _ = tx.send(native_window);
    } else {
        // Re-create after a config-change recreation should NOT happen with
        // the exhaustive `configChanges` declared on `ExtraWindowActivity`,
        // so this branch is a safety net only. Drop the NativeWindow; future
        // re-attach support lives in L7c.
        log::warn!(
            "multi_window: surfaceCreated re-arrived for windowId={window_id} \
             with no pending sender — config-change recreation? dropping surface"
        );
        drop(native_window);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraSurfaceChanged<
    'local,
>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    _surface: JObject<'local>,
    _format: i32,
    width: i32,
    height: i32,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraSurfaceChanged windowId={window_id} {width}x{height}");
    dispatch_event(ExtraWindowEvent::Resized {
        window_id,
        width: width.max(1) as u32,
        height: height.max(1) as u32,
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraSurfaceDestroyed<
    'local,
>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraSurfaceDestroyed windowId={window_id}");
    dispatch_event(ExtraWindowEvent::SurfaceDestroyed { window_id });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraTouchEvent<'local>(
    mut env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    action_masked: i32,
    action_index: i32,
    meta_state: i32,
    button_state: i32,
    event_time_millis: i64,
    vscroll: f32,
    hscroll: f32,
    xs: JFloatArray<'local>,
    ys: JFloatArray<'local>,
    pointer_ids: JIntArray<'local>,
) {
    let window_id = window_id as u64;
    log::info!(
        "multi_window: nativeOnExtraTouchEvent windowId={window_id} action={action_masked}"
    );
    let positions = match read_pointers(&mut env, &xs, &ys, &pointer_ids) {
        Ok(v) => v,
        Err(err) => {
            log::warn!("multi_window: failed to read pointers: {err:#}");
            return;
        }
    };
    dispatch_event(ExtraWindowEvent::Motion {
        window_id,
        action_masked,
        action_index,
        meta_state,
        button_state,
        event_time_millis,
        vscroll,
        hscroll,
        positions,
    });
}

/// JNI entry for `ExtraWindowActivity.dispatchKeyEvent`. Forwards the raw
/// Java `KeyEvent` fields onto the event channel so the platform's
/// `drain_extra_window_events` loop can route them to the right gpui
/// window on the game thread. Returns nothing — the Kotlin caller
/// always consumes the event regardless of whether the gpui side
/// produced a `PlatformInput` (we don't want Android's fallback IME
/// path stealing keystrokes from an editor that thinks it has focus).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraKeyEvent<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    action: i32,
    keycode: i32,
    meta_state: i32,
    repeat_count: i32,
) {
    let window_id = window_id as u64;
    log::debug!(
        "multi_window: nativeOnExtraKeyEvent windowId={window_id} action={action} \
         keycode={keycode} meta_state={meta_state:#x}"
    );
    dispatch_event(ExtraWindowEvent::Key {
        window_id,
        action,
        keycode: keycode as u32,
        meta_state: meta_state as u32,
        repeat_count,
    });
}

fn read_pointers(
    env: &mut jni::JNIEnv,
    xs: &JFloatArray,
    ys: &JFloatArray,
    pointer_ids: &JIntArray,
) -> Result<Vec<(f32, f32, i32)>> {
    let count = env.get_array_length(xs).context("xs length")? as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut x_buf = vec![0f32; count];
    let mut y_buf = vec![0f32; count];
    let mut id_buf = vec![0i32; count];
    env.get_float_array_region(xs, 0, &mut x_buf)
        .context("xs region")?;
    env.get_float_array_region(ys, 0, &mut y_buf)
        .context("ys region")?;
    env.get_int_array_region(pointer_ids, 0, &mut id_buf)
        .context("pointer_ids region")?;
    Ok(x_buf
        .into_iter()
        .zip(y_buf)
        .zip(id_buf)
        .map(|((x, y), id)| (x, y, id))
        .collect::<Vec<_>>())
}

#[allow(dead_code)]
fn _trait_check() {
    fn assert_send<T: Send>() {}
    assert_send::<NativeWindow>();
    assert_send::<ExtraWindowEvent>();
}
