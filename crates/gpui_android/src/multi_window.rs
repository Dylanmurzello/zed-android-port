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
//! the Activity's first `surfaceCreated` callback fires. A hard timeout
//! (`ACTIVITY_LAUNCH_TIMEOUT`) guards against ANR-class freezes if Android is
//! slow to launch the Activity.
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

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use android_activity::AndroidApp;
use android_activity::input::{MetaState, MotionAction, Source, ToolType};
use anyhow::{Context as _, Result, bail};
use futures::channel::{mpsc, oneshot};
use jni::objects::{GlobalRef, JFloatArray, JIntArray, JObject, JValue};
use ndk::native_window::NativeWindow;

use crate::events::{Motion, MotionPointer};

/// Hard cap on how long `create_extra_window_blocking` will wait for the
/// Activity's `surfaceCreated` callback. Cold Activity start in DeX desktop
/// windowing is ~530-700ms; the budget covers that plus headroom (see
/// `docs/workarounds/activity-launch-cold-timeout.md`). If exceeded we
/// return Err and gpui surfaces the failure to the caller (UX: "couldn't
/// open, try again") instead of freezing the Workspace forever. Yes 4s is
/// long; warm reopens are sub-100ms because the Activity stays alive in
/// Recents.
const ACTIVITY_LAUNCH_TIMEOUT: Duration = Duration::from_millis(4000);

/// How long `surfaceDestroyed` waits for the game thread to stop drawing.
const SURFACE_DETACH_TIMEOUT: Duration = Duration::from_millis(500);

/// Events from the JNI side (UI thread) that the gpui main thread needs to
/// process. Sent via `EVENT_TX`, drained in the platform's main loop.
pub(crate) enum ExtraWindowEvent {
    /// `surfaceChanged` for an existing extra window.
    Resized {
        window_id: u64,
        width: u32,
        height: u32,
    },
    /// `surfaceDestroyed`. The surface may only be used until that callback
    /// returns, and it waits for `detached` to send or drop.
    SurfaceDestroyed {
        window_id: u64,
        detached: std::sync::mpsc::Sender<()>,
    },
    /// A `surfaceCreated` after the first, once the Activity was stopped
    /// (screen off, minimized, covered) and started again.
    SurfaceCreated {
        window_id: u64,
        native_window: NativeWindow,
    },
    /// OS-initiated Activity destruction (user clicked the chrome X, or
    /// system swiped the task off Recents). The drain handler must invoke
    /// the gpui-registered `on_close` callback to drive `remove_window`,
    /// then reap our maps. The `GlobalRef` for the Activity has already been
    /// dropped from `EXTRA_ACTIVITY_REFS` by the time this event fires.
    OsClosed { window_id: u64 },
    /// A touch or pointer event from Java: any on an extra window's `SurfaceView`,
    /// and the main window's trackpad gestures. The platform translates it on the
    /// game thread.
    Motion { window_id: u64, motion: Motion },
    /// Hardware key event on the extra window. Raw fields from the Java
    /// `KeyEvent`; same off-thread translation policy as `Motion`. Without
    /// this routing the Editor in any extra window (Settings is the
    /// canonical case) never receives keystrokes — `set_input_handler`
    /// runs on focus, but no `PlatformInput::KeyDown` is ever fed in, so
    /// the search bar and any other Editor look frozen.
    Key {
        window_id: u64,
        action: i32,
        device_id: i32,
        keycode: u32,
        meta_state: u32,
        repeat_count: i32,
    },
    /// `onWindowFocusChanged` on the extra window's Activity.
    FocusChanged { window_id: u64, focused: bool },
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

/// Sender side of the event channel, set by [`init_event_channel`].
static EVENT_TX: Mutex<Option<mpsc::UnboundedSender<ExtraWindowEvent>>> = Mutex::new(None);

/// Sets up the event channel and returns the receiver the platform's run
/// loop drains.
pub(crate) fn init_event_channel() -> mpsc::UnboundedReceiver<ExtraWindowEvent> {
    let (tx, rx) = mpsc::unbounded();
    *EVENT_TX.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    rx
}

fn pending_table()
-> std::sync::MutexGuard<'static, Option<HashMap<u64, oneshot::Sender<NativeWindow>>>> {
    SURFACE_CREATED_TX
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn refs_table() -> std::sync::MutexGuard<'static, Option<HashMap<u64, GlobalRef>>> {
    EXTRA_ACTIVITY_REFS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Clone the `GlobalRef` for the `ExtraWindowActivity` with this
/// `window_id`. Returns `None` if the Activity isn't registered
/// (already destroyed, or never created). Callers that need to call
/// Kotlin methods on a specific extra Activity (per-window IME mode,
/// etc.) go through this.
pub(crate) fn extra_activity_for(window_id: u64) -> Option<GlobalRef> {
    refs_table()
        .as_ref()
        .and_then(|m| m.get(&window_id))
        .cloned()
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

fn registered_set() -> std::sync::MutexGuard<'static, Option<std::collections::HashSet<u64>>> {
    REGISTERED_WINDOWS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
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
///
/// `wait` runs between checks, for at most the given time, and returns
/// whether to keep waiting. It must handle the main Activity's lifecycle
/// commands: when the new Activity covers the main one, Android pauses the
/// main Activity first, and GameActivity holds the UI thread until this
/// thread acknowledges the pause. The new Activity starts on that same UI
/// thread, so without the acknowledgement its surface only arrives once
/// GameActivity gives up, after 2 seconds.
pub(crate) fn create_extra_window_blocking(
    android_app: &AndroidApp,
    window_id: u64,
    bounds: Option<LaunchBounds>,
    mut wait: impl FnMut(Duration) -> bool,
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
    // thread forever (see `ACTIVITY_LAUNCH_TIMEOUT`).
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
        let timed_out = std::time::Instant::now() >= deadline;
        if timed_out || !wait(Duration::from_millis(8)) {
            pending_table().as_mut().and_then(|m| m.remove(&window_id));
            // Closing makes a send from now on fail, so the UI thread hands
            // the surface to `drain_extra_window_events` instead; one that
            // arrived since the check above is still here.
            rx.close();
            if let Ok(Some(native_window)) = rx.try_recv() {
                return Ok(native_window);
            }
            // Otherwise the Activity may still start later, with no window
            // to draw; `drain_extra_window_events` finishes it then.
            if timed_out {
                bail!(
                    "ExtraWindowActivity startup exceeded {}ms",
                    ACTIVITY_LAUNCH_TIMEOUT.as_millis()
                );
            }
            bail!("the app is shutting down");
        }
    }
}

/// Brings the window's Activity to the front, for gpui's `activate_window`:
/// Zed uses it to surface an open Settings window instead of opening another.
pub(crate) fn activate_extra_activity(android_app: &AndroidApp, window_id: u64) {
    let result = crate::jni_env::call_window_method(
        android_app,
        Some(window_id),
        "moveOwnTaskToFront",
        "()V",
        &[],
    );
    if let Err(err) = result {
        log::warn!("multi_window: activate_extra_activity({window_id}): {err:#}");
    }
}

/// Set the title on the `ExtraWindowActivity` for `window_id`. Shows up in the
/// OS chrome's drag bar (in freeform/desktop windowing) and in Recents.
pub(crate) fn set_extra_activity_title(android_app: &AndroidApp, window_id: u64, title: &str) {
    let result = crate::jni_env::with_env(android_app, |env| {
        let title = env.new_string(title)?;
        crate::jni_env::call_window_method(
            android_app,
            Some(window_id),
            "setWindowTitle",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&title)],
        )
    });
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
    let bounds = bounds.unwrap_or(LaunchBounds {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    });
    crate::jni_env::with_activity(android_app, |env, activity| {
        env.call_method(
            activity,
            "launchExtraWindow",
            "(JIIII)V",
            &[
                JValue::Long(window_id as i64),
                JValue::Int(bounds.left),
                JValue::Int(bounds.top),
                JValue::Int(bounds.right),
                JValue::Int(bounds.bottom),
            ],
        )?;
        Ok(())
    })
}

fn call_finish_and_remove_task(android_app: &AndroidApp, activity: &GlobalRef) -> Result<()> {
    crate::jni_env::with_env(android_app, |env| {
        env.call_method(activity.as_obj(), "finishAndRemoveTask", "()V", &[])?;
        Ok(())
    })
}

fn dispatch_event(event: ExtraWindowEvent) {
    let guard = EVENT_TX.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(tx) = guard.as_ref() else {
        log::warn!("multi_window: event arrived before init_event_channel");
        return;
    };
    if let Err(err) = tx.unbounded_send(event) {
        log::warn!("multi_window: dispatch_event: {err:#}");
    }
    crate::platform::wake_run_loop();
}

/// Process-death recovery probe. Called by `ExtraWindowActivity.onCreate`
/// before any other JNI work. Returns true if the gpui-side has a live
/// AndroidWindow registered for this `windowId` — i.e. this Activity was
/// launched in the current Rust process and gpui knows about it. Returns
/// false on resurrection (the OS brought the Activity back from Recents
/// but the process was killed and restarted, so gpui has no record).
/// Activity uses the result to decide whether to proceed or `finish()`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeIsExtraWindowKnown<'local>(
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
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraActivityCreated<'local>(
    env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    activity: JObject<'local>,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraActivityCreated windowId={window_id}");
    let global_ref = match env.new_global_ref(activity) {
        Ok(r) => r,
        Err(err) => {
            log::error!(
                "multi_window: failed to create GlobalRef for activity windowId={window_id}: {err:#}"
            );
            return;
        }
    };
    refs_table()
        .get_or_insert_with(HashMap::new)
        .insert(window_id, global_ref);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraActivityDestroyed<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraActivityDestroyed windowId={window_id}");
    // Dropped here, on a JVM thread. The OsClosed event then tears the window
    // down on the game thread.
    drop(refs_table().as_mut().and_then(|m| m.remove(&window_id)));
    crate::insets::forget(window_id);
    dispatch_event(ExtraWindowEvent::OsClosed { window_id });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraSurfaceCreated<'local>(
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
    match pending {
        Some(tx) => {
            // The wait timed out between taking the sender and sending;
            // the run loop finishes the Activity of an unknown window.
            if let Err(native_window) = tx.send(native_window) {
                dispatch_event(ExtraWindowEvent::SurfaceCreated {
                    window_id,
                    native_window,
                });
            }
        }
        None => dispatch_event(ExtraWindowEvent::SurfaceCreated {
            window_id,
            native_window,
        }),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraSurfaceChanged<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    _surface: JObject<'local>,
    _format: i32,
    width: i32,
    height: i32,
) {
    let window_id = window_id as u64;
    log::debug!("multi_window: nativeOnExtraSurfaceChanged windowId={window_id} {width}x{height}");
    dispatch_event(ExtraWindowEvent::Resized {
        window_id,
        width: width.max(1) as u32,
        height: height.max(1) as u32,
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraSurfaceDestroyed<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) {
    let window_id = window_id as u64;
    log::info!("multi_window: nativeOnExtraSurfaceDestroyed windowId={window_id}");
    let (detached, wait) = std::sync::mpsc::channel();
    dispatch_event(ExtraWindowEvent::SurfaceDestroyed {
        window_id,
        detached,
    });
    // The game thread may be opening a window, which waits for this UI thread,
    // or in a long frame, so the wait is short and Android gets its callback
    // back regardless.
    if wait.recv_timeout(SURFACE_DETACH_TIMEOUT) == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    {
        log::warn!("multi_window: window {window_id} didn't stop drawing in time");
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnExtraFocusChanged<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    has_focus: jni::sys::jboolean,
) {
    let window_id = window_id as u64;
    let focused = has_focus != 0;
    log::debug!("multi_window: nativeOnExtraFocusChanged windowId={window_id} focused={focused}");
    dispatch_event(ExtraWindowEvent::FocusChanged { window_id, focused });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeOnMotionEvent<'local>(
    mut env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    action_masked: i32,
    action_index: i32,
    source: i32,
    button_state: i32,
    meta_state: i32,
    flags: i32,
    classification: i32,
    vscroll: f32,
    hscroll: f32,
    xs: JFloatArray<'local>,
    ys: JFloatArray<'local>,
    pointer_ids: JIntArray<'local>,
    tool_types: JIntArray<'local>,
) {
    let window_id = window_id as u64;
    log::debug!("multi_window: nativeOnMotionEvent windowId={window_id} action={action_masked}");
    let pointers = match read_pointers(&mut env, &xs, &ys, &pointer_ids, &tool_types) {
        Ok(pointers) => pointers,
        Err(err) => {
            log::warn!("multi_window: failed to read pointers: {err:#}");
            return;
        }
    };
    dispatch_event(ExtraWindowEvent::Motion {
        window_id,
        motion: Motion {
            action: MotionAction::from(action_masked as u32),
            action_index: action_index.max(0) as usize,
            source: Source::from(source as u32),
            meta_state: MetaState(meta_state as u32),
            button_state: button_state as u32,
            canceled: flags as u32 & crate::events::FLAG_CANCELED != 0,
            classification,
            vscroll,
            hscroll,
            pointers,
        },
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
    device_id: i32,
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
        device_id,
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
    tool_types: &JIntArray,
) -> Result<Vec<MotionPointer>> {
    let count = env.get_array_length(xs).context("xs length")? as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut x_buf = vec![0f32; count];
    let mut y_buf = vec![0f32; count];
    let mut id_buf = vec![0i32; count];
    let mut tool_type_buf = vec![0i32; count];
    env.get_float_array_region(xs, 0, &mut x_buf)
        .context("xs region")?;
    env.get_float_array_region(ys, 0, &mut y_buf)
        .context("ys region")?;
    env.get_int_array_region(pointer_ids, 0, &mut id_buf)
        .context("pointer_ids region")?;
    env.get_int_array_region(tool_types, 0, &mut tool_type_buf)
        .context("tool_types region")?;
    Ok(x_buf
        .into_iter()
        .zip(y_buf)
        .zip(id_buf)
        .zip(tool_type_buf)
        .map(|(((x, y), id), tool)| MotionPointer {
            id,
            position: gpui::point(gpui::px(x), gpui::px(y)),
            tool: ToolType::from(tool as u32),
        })
        .collect())
}
