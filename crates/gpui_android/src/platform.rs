use std::{
    cell::RefCell,
    ffi::c_void,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use android_activity::AndroidApp;
use ndk::configuration::UiModeNight;
use anyhow::Result;
use futures::channel::oneshot;
use gpui::{
    Action, AnyWindowHandle, BackgroundExecutor, Bounds, ClipboardItem, CursorStyle,
    DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu, MenuItem, PathPromptOptions, Pixels,
    Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, PriorityQueueReceiver, RunnableVariant, Task, ThermalState, WindowAppearance,
    WindowParams,
};
use gpui_wgpu::GpuContext;

use crate::dispatcher::AndroidDispatcher;
use crate::display::AndroidDisplay;
use crate::keyboard::AndroidKeyboardLayout;
use crate::window::{AndroidWindow, AndroidWindowStatePtr};

/// AChoreographer FFI — NDK API 24+. We avoid going through Java's
/// android.view.Choreographer because we'd need a JNI hop on every
/// vsync; the NDK exposes the same scheduler natively. Linked from
/// libandroid.so which Android already keeps mapped.
#[link(name = "android")]
unsafe extern "C" {
    fn AChoreographer_getInstance() -> *mut c_void;
    fn AChoreographer_postFrameCallback64(
        choreographer: *mut c_void,
        callback: ChoreographerFrameCallback,
        data: *mut c_void,
    );
}

type ChoreographerFrameCallback =
    unsafe extern "C" fn(frame_time_nanos: i64, data: *mut c_void);

/// `ANativeWindow_setFrameRate` is NDK API 30+. minSdk is 26, so we
/// can't direct-link the symbol (it would `dlopen`-fail at app load on
/// 26-29 devices and crash before any code runs). Resolve it lazily
/// via `dlsym` against the already-loaded libandroid.so; absent symbol
/// just means we stay at the panel's compatibility default (typically
/// 60Hz on devices new enough to have 120Hz panels but old enough to
/// lack this API — a narrow band).
type SetFrameRateFn =
    unsafe extern "C" fn(window: *mut c_void, frame_rate: f32, compatibility: i8) -> i32;

/// `ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_DEFAULT`. Tells the system
/// "I want at least this rate; the system may choose to render
/// higher if it benefits the user." The right hint for an interactive
/// app where smoother is always better.
const FRAME_RATE_COMPATIBILITY_DEFAULT: i8 = 0;

/// Target frame rate. Samsung Tab S9 Ultra / Pixel Tablet support
/// 120Hz panels; mid-range devices typically expose 90Hz; older
/// devices stay at 60Hz. When the panel can't deliver the requested
/// rate the compositor downgrades transparently, so hardcoding the
/// max we care about is safe across the device matrix. Floating-point
/// because Android's API takes float.
const TARGET_FRAME_RATE_HZ: f32 = 120.0;

/// Ask the system to render the given native window at our target
/// refresh rate. No-op on API 26-29 (returns `false`).
///
/// CRITICAL: passing `0.0` Hz here means "I'm inactive / I have no
/// preference" — Samsung's adaptive panel reads that as a hint to drop
/// to its idle rate (30Hz on Tab S9 Ultra), producing visible 33ms
/// vsync intervals and the user-visible stutter that motivated this
/// fix. Always pass a positive frame rate so the panel knows we're an
/// active app that wants high refresh.
///
/// The pointer must be a valid `ANativeWindow*` for at least the
/// duration of this call. Caller (window.rs::attach_surface) holds an
/// `ANativeWindow_acquire` refcount through the surrounding `NativeWindow`
/// wrapper, so the lifetime requirement is satisfied trivially.
pub(crate) fn set_native_window_frame_rate(window: *mut c_void) -> bool {
    if window.is_null() {
        return false;
    }
    // RTLD_NOLOAD asks "is this already loaded?" without mapping it
    // afresh. libandroid.so is always loaded on Android (Choreographer
    // FFI above depends on it) so this is effectively a handle fetch.
    let lib = unsafe {
        libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOLOAD | libc::RTLD_LAZY)
    };
    if lib.is_null() {
        log::warn!("set_native_window_frame_rate: libandroid.so not loaded; skipping");
        return false;
    }
    let sym = unsafe { libc::dlsym(lib, c"ANativeWindow_setFrameRate".as_ptr()) };
    if sym.is_null() {
        log::info!(
            "ANativeWindow_setFrameRate unavailable (API < 30); panel \
             stays at compatibility-default refresh"
        );
        return false;
    }
    let set_frame_rate: SetFrameRateFn = unsafe { std::mem::transmute(sym) };
    let result = unsafe {
        set_frame_rate(window, TARGET_FRAME_RATE_HZ, FRAME_RATE_COMPATIBILITY_DEFAULT)
    };
    if result == 0 {
        log::info!(
            "ANativeWindow_setFrameRate({TARGET_FRAME_RATE_HZ}, DEFAULT) accepted; \
             panel will run at the highest available rate up to the target"
        );
        true
    } else {
        log::warn!("ANativeWindow_setFrameRate returned error code {result}");
        false
    }
}

/// Set by the Choreographer callback (called once per vsync on the
/// main thread's looper, so no synchronization beyond the atomic).
/// Drained at the top of each event-loop tick to decide whether to
/// call window.refresh().
static FRAME_PENDING: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn frame_callback(_frame_time_nanos: i64, _data: *mut c_void) {
    FRAME_PENDING.store(true, Ordering::Release);
    crate::frame_timing::vsync_arrived();
    // Re-post for the next vsync so we get a continuous stream of
    // frame callbacks. Stopping this stream means the next tick won't
    // get a vsync wake-up; we always want to be ready to render.
    unsafe {
        let c = AChoreographer_getInstance();
        if !c.is_null() {
            AChoreographer_postFrameCallback64(
                c,
                frame_callback,
                std::ptr::null_mut(),
            );
        }
    }
}

/// Register the first Choreographer frame callback on the calling
/// thread (must have a Looper attached — android-activity's main
/// thread does). Subsequent callbacks self-re-post inside
/// `frame_callback` so we keep getting vsync ticks for the app's
/// lifetime.
fn install_choreographer_callback() {
    unsafe {
        let c = AChoreographer_getInstance();
        if c.is_null() {
            log::warn!(
                "AChoreographer_getInstance returned null; vsync sync \
                 disabled, falling back to poll-timeout-driven refresh"
            );
            return;
        }
        AChoreographer_postFrameCallback64(c, frame_callback, std::ptr::null_mut());
    }
    log::info!("AndroidPlatform: Choreographer frame callback registered");
}

#[derive(Default)]
pub(crate) struct PlatformHandlers {
    pub(crate) open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    pub(crate) quit: Option<Box<dyn FnMut()>>,
    pub(crate) reopen: Option<Box<dyn FnMut()>>,
    pub(crate) app_menu_action: Option<Box<dyn FnMut(&dyn Action)>>,
    pub(crate) will_open_app_menu: Option<Box<dyn FnMut()>>,
    pub(crate) validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    pub(crate) keyboard_layout_change: Option<Box<dyn FnMut()>>,
}

pub(crate) struct AndroidCommon {
    pub(crate) background_executor: BackgroundExecutor,
    pub(crate) foreground_executor: ForegroundExecutor,
    pub(crate) text_system: Arc<dyn PlatformTextSystem>,
    pub(crate) appearance: WindowAppearance,
    pub(crate) callbacks: PlatformHandlers,
    pub(crate) main_receiver: PriorityQueueReceiver<RunnableVariant>,
    pub(crate) active_window: Option<AnyWindowHandle>,
    pub(crate) gpu_context: GpuContext,
    /// The GameActivity-owned primary window. First `cx.open_window` lands
    /// here; subsequent calls go to [`extra_windows`].
    pub(crate) window: Option<AndroidWindowStatePtr>,
    /// Secondary `cx.open_window` results, keyed by `WindowId::as_u64()`.
    /// Each is hosted in its own `ExtraWindowActivity`; the OS owns the
    /// chrome and routes touch/keyboard input straight to that Activity's
    /// `SurfaceView`. We just need the state ptr to dispatch resize and
    /// motion events delivered through the `extra_event_rx` channel.
    pub(crate) extra_windows: std::collections::HashMap<u64, AndroidWindowStatePtr>,
    /// Receiver side of the JNI → game-thread channel for extra-window
    /// events. Drained each iteration of the platform run loop. `Some`
    /// from `AndroidCommon::new` until the loop terminates.
    pub(crate) extra_event_rx: Option<futures::channel::mpsc::UnboundedReceiver<crate::multi_window::ExtraWindowEvent>>,
    /// Receiver side of the JNI → game-thread channel for captured
    /// pointer events. Populated when `MainActivity` activates pointer
    /// capture; each captured `MotionEvent` is marshaled across JNI
    /// and lands here for `translate` + dispatch.
    pub(crate) captured_pointer_rx:
        Option<futures::channel::mpsc::UnboundedReceiver<crate::captured_pointer::CapturedEvent>>,
    /// Receiver side of the JNI → game-thread channel for IME events.
    /// Each event carries the originating window id (0 = primary
    /// `MainActivity`, otherwise the `ExtraWindowActivity` window id
    /// gpui assigned). Drained each iteration of the platform run
    /// loop; `crate::ime::drain_ime_events` routes per-id into the
    /// right window's `PlatformInputHandler`.
    pub(crate) ime_event_rx: Option<
        futures::channel::mpsc::UnboundedReceiver<(u64, crate::ime::ImeEvent)>,
    >,
    /// Tracks whether the soft keyboard was visible last tick.
    /// When this disagrees with the atomic Kotlin pushes via
    /// `nativeSetSoftKeyboardVisible`, we force a `window.refresh()`
    /// so the pane keyboard button re-renders with its updated
    /// `toggle_state`. Without this, the atomic flips on each
    /// toggle but the UI keeps showing stale state until some
    /// other event triggers a paint — the user-visible bug where
    /// the button needs two taps to "respond".
    pub(crate) last_soft_keyboard_visible: bool,
    /// Tracks the last-observed `TRACKPAD_MODE_ENABLED` value so we
    /// can JNI-push the show/hide of the SurfaceControl cursor
    /// overlay on the Kotlin side only when the mode actually flips.
    /// Avoids burning a JNI call every tick.
    pub(crate) last_trackpad_mode_enabled: bool,
    /// Mirrors the last-pushed value of `EXTRAS_ROW_ENABLED` to
    /// Kotlin. `None` until the first push; the first tick after
    /// startup always pushes the current value, then subsequent
    /// ticks edge-detect. Plain-`bool` initialized to `!current`
    /// missed the sync when the persisted setting in settings.json
    /// matched the runtime atomic default (both `true`), so Kotlin
    /// never learned the user's value until they toggled it.
    pub(crate) last_extras_row_enabled: Option<bool>,
    /// Edge-detection slot for the soft-keyboard setting push. Same
    /// pattern as `last_extras_row_enabled`: `None` until the first
    /// tick, then `Some(current)` after each successful push. Drives
    /// `ImeHostView.onCheckIsTextEditor` on the Kotlin side so
    /// Android stops treating the IME host as a text editor (and
    /// therefore stops auto-showing Gboard on focus) when the user
    /// has the setting off.
    pub(crate) last_soft_keyboard_setting: Option<bool>,
    pub(crate) running: bool,
}

impl AndroidCommon {
    pub fn new(android_app: &AndroidApp) -> Self {
        let (dispatcher, main_receiver) = AndroidDispatcher::new(android_app);
        let dispatcher = Arc::new(dispatcher);

        // `new_without_system_fonts` skips fontdb's automatic
        // platform-font discovery (which on Android scans /system/fonts/
        // anyway, but we previously avoided it because the cost was
        // unbounded). Now we opt back in to /system/fonts/ specifically
        // — that dir alone is well-bounded (~70 .ttf/.otf files, ~60ms
        // first-load on Snapdragon 8 Gen 2) and surfaces NotoSansSymbols2,
        // NotoColorEmoji, DroidSansMono, plus the full Noto family for
        // CJK / Cyrillic / Arabic / etc. Without this fallback, prompts
        // and statuslines that use powerline glyphs (`❯` U+276F),
        // dingbats (`✻` U+273B), or any non-Latin codepoint render as
        // tofu boxes because Lilex + IBM Plex Sans only cover basic
        // Latin / extended Latin.
        let cosmic = gpui_wgpu::CosmicTextSystem::new_without_system_fonts("Lilex");
        cosmic.load_fonts_dir("/system/fonts");
        let text_system: Arc<dyn PlatformTextSystem> = Arc::new(cosmic);

        Self {
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher),
            text_system,
            appearance: appearance_from_config(android_app),
            callbacks: PlatformHandlers::default(),
            main_receiver,
            active_window: None,
            gpu_context: Rc::new(RefCell::new(None)),
            window: None,
            extra_windows: std::collections::HashMap::new(),
            extra_event_rx: Some(crate::multi_window::init_event_channel()),
            captured_pointer_rx: Some(crate::captured_pointer::init_event_channel()),
            ime_event_rx: Some(crate::ime::init_event_channel()),
            last_soft_keyboard_visible: false,
            last_trackpad_mode_enabled: false,
            last_extras_row_enabled: None,
            last_soft_keyboard_setting: None,
            running: true,
        }
    }
}

fn appearance_from_config(android_app: &AndroidApp) -> WindowAppearance {
    match android_app.config().ui_mode_night() {
        UiModeNight::Yes => WindowAppearance::Dark,
        _ => WindowAppearance::Light,
    }
}

/// Dispatch `MainActivity.openUrl(url: String)` via JNI. Backs
/// `AndroidPlatform::open_url`; the upstream gpui trait stub was empty so
/// `cx.open_url(...)` was a silent no-op on Android. The Kotlin side wraps
/// `startActivity(Intent.ACTION_VIEW, Uri.parse(url))` and swallows
/// ActivityNotFoundException.
fn jni_open_url(android_app: &AndroidApp, url: &str) -> Result<()> {
    use anyhow::Context;
    use jni::{JavaVM, objects::JObject};

    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm
        .attach_current_thread()
        .context("attach_current_thread for open_url")?;
    let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as _) };
    let url_jstring = env
        .new_string(url)
        .context("alloc JString for open_url argument")?;
    env.call_method(
        &activity,
        "openUrl",
        "(Ljava/lang/String;)V",
        &[(&url_jstring).into()],
    )
    .context("MainActivity.openUrl")?;
    Ok(())
}

pub struct AndroidPlatform {
    pub(crate) common: RefCell<AndroidCommon>,
    pub(crate) android_app: AndroidApp,
}

impl AndroidPlatform {
    pub fn new(android_app: AndroidApp, _headless: bool) -> Self {
        let common = AndroidCommon::new(&android_app);
        Self {
            common: RefCell::new(common),
            android_app,
        }
    }

    /// Translate the gpui-side `WindowParams.bounds` (in logical pixels at
    /// the primary's scale factor) into device-pixel `Rect` coordinates the
    /// OS will use as the freeform window's initial size and position. Falls
    /// back to `None` (let the OS pick) if either the requested size is
    /// nonsense or we can't read screen dimensions.
    fn compute_launch_bounds(
        &self,
        bounds: &Bounds<Pixels>,
        scale_factor: f32,
    ) -> Option<crate::multi_window::LaunchBounds> {
        let width_px = (bounds.size.width.as_f32() * scale_factor).round() as i32;
        let height_px = (bounds.size.height.as_f32() * scale_factor).round() as i32;
        if width_px <= 0 || height_px <= 0 {
            return None;
        }
        let nw = self.android_app.native_window()?;
        let screen_w = nw.width() as i32;
        let screen_h = nw.height() as i32;
        if screen_w <= 0 || screen_h <= 0 {
            return None;
        }
        // Center the window on screen by default. Caller-supplied origin is
        // ignored for now — gpui's WindowParams.bounds.origin is meaningless
        // on Android (no window manager coordinate space prior to L7e).
        let left = ((screen_w - width_px) / 2).max(0);
        let top = ((screen_h - height_px) / 2).max(0);
        Some(crate::multi_window::LaunchBounds {
            left,
            top,
            right: left + width_px,
            bottom: top + height_px,
        })
    }

    /// Read the device's display density and convert to a scale factor.
    /// Android reports density in dpi where 160 dpi = 1.0x. Tab S9 Ultra
    /// reports ~336 dpi (~2.1x). Falls back to 1.0 if the density isn't yet
    /// reported (e.g. very early in startup before the first config arrives).
    fn compute_scale_factor(&self) -> f32 {
        match self.android_app.config().density() {
            Some(dpi) if dpi > 0 => (dpi as f32 / 160.0).max(1.0),
            _ => 1.0,
        }
    }

    /// Branch of [`open_window`](Self::open_window) for the second-and-beyond
    /// `cx.open_window` call. Launches an `ExtraWindowActivity` via JNI Intent,
    /// waits up to 500ms for its `surfaceCreated` callback, then wraps the
    /// resulting `ANativeWindow` in a new `AndroidWindow`. The OS owns the
    /// chrome and routes touch/lifecycle events to that Activity's
    /// `SurfaceView`; events flow through `drain_extra_window_events`.
    ///
    /// `options.bounds` is informational only — actual placement is
    /// controlled by the OS window manager (or by ActivityOptions launch
    /// bounds, deferred to L7e).
    fn open_extra_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let scale_factor = self.compute_scale_factor();
        let window_id = handle.window_id().as_u64();
        let launch_bounds = self.compute_launch_bounds(&options.bounds, scale_factor);
        log::info!(
            "open_extra_window: windowId={window_id} launching ExtraWindowActivity bounds={launch_bounds:?}"
        );

        // Mark the window as known BEFORE launching the Activity. If we
        // marked it later (e.g. after `attach_surface`), the Activity's
        // `onCreate` would race ahead and call `nativeIsExtraWindowKnown`
        // before the mark — getting a false negative and finishing itself
        // prematurely. The `unmark_window_registered` calls below cover the
        // failure paths.
        crate::multi_window::mark_window_registered(window_id);

        let native_window = match crate::multi_window::create_extra_window_blocking(
            &self.android_app,
            window_id,
            launch_bounds,
        ) {
            Ok(nw) => nw,
            Err(err) => {
                crate::multi_window::unmark_window_registered(window_id);
                return Err(err);
            }
        };

        let appearance = self.common.borrow().appearance;
        let gpu_context = self.common.borrow().gpu_context.clone();
        let mut window =
            AndroidWindow::new(handle, options, gpu_context, appearance, self.android_app.clone());
        window.extra_window_id = Some(window_id);
        // Mirror to the state so the touch / trackpad dispatchers
        // (which receive `&mut AndroidWindowState`, not the wrapping
        // `AndroidWindow`) know which Activity owns this window.
        window
            .ptr()
            .state
            .borrow_mut()
            .extra_window_id = Some(window_id);
        if let Err(err) = window.ptr().attach_surface(native_window, scale_factor) {
            crate::multi_window::unmark_window_registered(window_id);
            return Err(err);
        }

        {
            let mut common = self.common.borrow_mut();
            common.extra_windows.insert(window_id, window.ptr());
            common.active_window = Some(handle);
        }
        // Fire the activation observers so the editor's
        // `cx.observe_window_activation` callback runs and enables cursor
        // blink (and any other activation-gated state). Our `is_active()`
        // returns true at construction, but gpui's observer machinery only
        // fires when the platform calls the active-status-change callback;
        // without this, the search field's cursor renders statically until
        // the user's first input. See `blink_manager.rs` and `editor.rs`'s
        // `cx.observe_window_activation` registration.
        //
        // Must defer to a later tick: gpui registers the
        // `on_active_status_change` callback inside `Window::new`, which
        // runs AFTER our `open_extra_window` returns. Firing synchronously
        // here would no-op against an empty callback slot.
        let executor = self.common.borrow().foreground_executor.clone();
        let window_ptr = window.ptr();
        executor
            .spawn(async move {
                window_ptr.notify_active_status_change(true);
            })
            .detach();
        Ok(Box::new(window))
    }

    /// Pull JNI-originated extra-window events off the channel and dispatch
    /// to the matching `AndroidWindowStatePtr`. Called once per iteration of
    /// the platform's main loop.
    fn drain_extra_window_events(&self) {
        use crate::multi_window::ExtraWindowEvent;

        let mut rx = match self.common.borrow_mut().extra_event_rx.take() {
            Some(rx) => rx,
            None => return,
        };

        while let Ok(event) = rx.try_recv() {
            match event {
                ExtraWindowEvent::Resized {
                    window_id,
                    width,
                    height,
                } => {
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    if let Some(state) = entry {
                        let scale = state.state.borrow().scale_factor;
                        state.resize_surface(width, height, scale);
                    } else {
                        log::warn!(
                            "drain_extra_window_events: Resized for unknown windowId={window_id}"
                        );
                    }
                }
                ExtraWindowEvent::SurfaceDestroyed { window_id } => {
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    if let Some(state) = entry {
                        state.detach_surface();
                    }
                }
                ExtraWindowEvent::OsClosed { window_id } => {
                    // OS-initiated close (user clicked chrome X). Set the
                    // os_closed flag so AndroidWindow::Drop will skip its
                    // JNI finishActivity call (Activity is already gone),
                    // then fire the gpui-registered `on_close` callback.
                    // gpui's callback drives `Window::remove_window()`, which
                    // in turn drops our `Box<dyn PlatformWindow>` and
                    // ultimately reaps the `extra_windows` map entry.
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    let Some(state) = entry else {
                        log::info!(
                            "drain_extra_window_events: OsClosed for already-removed windowId={window_id}"
                        );
                        crate::multi_window::unmark_window_registered(window_id);
                        continue;
                    };
                    state.state.borrow().os_closed.store(true, std::sync::atomic::Ordering::SeqCst);
                    let close_cb = state.callbacks.borrow_mut().close.take();
                    if let Some(cb) = close_cb {
                        log::info!(
                            "drain_extra_window_events: OsClosed windowId={window_id} → invoking gpui on_close"
                        );
                        cb();
                    } else {
                        // No callback registered — gpui never wired one. Tear
                        // down the platform-side state directly so we don't
                        // leak.
                        log::warn!(
                            "drain_extra_window_events: OsClosed windowId={window_id} but no on_close registered"
                        );
                        let mut common = self.common.borrow_mut();
                        common.extra_windows.remove(&window_id);
                        if common
                            .active_window
                            .as_ref()
                            .is_some_and(|h| h.window_id().as_u64() == window_id)
                        {
                            common.active_window = None;
                        }
                    }
                    crate::multi_window::unmark_window_registered(window_id);
                }
                ExtraWindowEvent::Motion {
                    window_id,
                    action_masked,
                    action_index,
                    meta_state,
                    button_state,
                    event_time_millis: _,
                    vscroll,
                    hscroll,
                    positions,
                } => {
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    let Some(state) = entry else {
                        log::warn!(
                            "drain_extra_window_events: Motion for unknown windowId={window_id}"
                        );
                        continue;
                    };
                    let inputs = {
                        let mut window_state = state.state.borrow_mut();
                        let scale = window_state.scale_factor;
                        crate::events::translate_extra_motion_event(
                            window_id,
                            action_masked,
                            action_index,
                            meta_state,
                            button_state,
                            vscroll,
                            hscroll,
                            &positions,
                            scale,
                            &mut window_state,
                        )
                    };
                    for input in inputs {
                        state.handle_input(input);
                    }
                }
                ExtraWindowEvent::Key {
                    window_id,
                    action,
                    keycode,
                    meta_state,
                    repeat_count,
                } => {
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    let Some(state) = entry else {
                        log::warn!(
                            "drain_extra_window_events: Key for unknown windowId={window_id}"
                        );
                        continue;
                    };
                    if let Some(input) = crate::events::translate_extra_key_event(
                        action,
                        keycode,
                        meta_state,
                        repeat_count,
                    ) {
                        state.handle_input(input);
                    }
                }
            }
        }

        self.common.borrow_mut().extra_event_rx = Some(rx);
    }

    /// Pull captured-pointer events (raw trackpad input while
    /// MainActivity has pointer capture) off the JNI channel, run them
    /// through the synthesis state machine, and feed the resulting
    /// `PlatformInput`s into the primary window. Cursor position is
    /// tracked inside `captured_pointer::translate`; we just need to
    /// give it the current window size so it can clamp.
    fn drain_captured_pointer_events(&self) {
        let mut rx = match self.common.borrow_mut().captured_pointer_rx.take() {
            Some(rx) => rx,
            None => return,
        };
        while let Ok(event) = rx.try_recv() {
            // Route the event to whichever window it came from.
            // `PRIMARY_WINDOW_ID` (0) → MainActivity's primary window;
            // anything else → look up in extra_windows. If the window
            // can't be found we drop the event (window torn down or
            // not yet attached); dropping is preferable to panicking
            // on a transient lookup miss.
            let target = if event.window_id == crate::captured_pointer::PRIMARY_WINDOW_ID {
                self.common.borrow().window.clone()
            } else {
                self.common
                    .borrow()
                    .extra_windows
                    .get(&event.window_id)
                    .cloned()
            };
            let Some(window_ptr) = target else { continue };
            let inputs = {
                let mut window_state = window_ptr.state.borrow_mut();
                // Captured-trackpad is mouse-like; clear the
                // last-input-was-touch flag so any subsequent UI
                // hit-test falls back to the precise mouse path.
                window_state
                    .last_input_was_touch
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let scale_factor = window_state.scale_factor;
                crate::captured_pointer::translate(
                    &mut window_state.captured,
                    event,
                    scale_factor,
                )
            };
            for input in inputs {
                window_ptr.handle_input(input);
            }
        }
        self.common.borrow_mut().captured_pointer_rx = Some(rx);
    }

    /// Drain queued IME events posted from `ZdroidInputConnection` on
    /// Android's UI thread. Each event carries the originating
    /// window's id (0 = MainActivity, otherwise the ExtraWindow id)
    /// and routes into that window's `PlatformInputHandler`.
    fn drain_ime_events(&self) {
        let mut rx = match self.common.borrow_mut().ime_event_rx.take() {
            Some(rx) => rx,
            None => return,
        };
        let (primary_window, extras) = {
            let common = self.common.borrow();
            (common.window.clone(), common.extra_windows.clone())
        };
        crate::ime::drain_ime_events(
            primary_window.as_ref(),
            |id| extras.get(&id).cloned(),
            &mut rx,
        );
        self.common.borrow_mut().ime_event_rx = Some(rx);
    }

    /// Sample every window's text-input focus at frame boundary
    /// and drive show / hide IME (per window) when it changes. The
    /// per-window state lives on `AndroidWindowState`, so each
    /// `ExtraWindowActivity` can drive its own IME independent of
    /// the primary surface — required for settings / picker windows
    /// that own their own InputConnection and need the soft
    /// keyboard to follow their focus instead of MainActivity's.
    fn reconcile_ime_visibility(&self) {
        let windows: Vec<(Option<u64>, crate::window::AndroidWindowStatePtr)> = {
            let common = self.common.borrow();
            let mut all = Vec::new();
            if let Some(primary) = common.window.clone() {
                all.push((None, primary));
            }
            for (id, ptr) in common.extra_windows.iter() {
                all.push((Some(*id), ptr.clone()));
            }
            all
        };
        for (extra_window_id, window_ptr) in windows {
            self.reconcile_ime_for_window(extra_window_id, &window_ptr);
        }
    }

    fn reconcile_ime_for_window(
        &self,
        extra_window_id: Option<u64>,
        window_ptr: &crate::window::AndroidWindowStatePtr,
    ) {
        let (currently_visible, app, was_visible) = {
            let state = window_ptr.state.borrow();
            (
                state.input_handler.is_some(),
                state.android_app.clone(),
                state.ime_currently_visible,
            )
        };
        let visibility_changed = currently_visible != was_visible;
        if visibility_changed {
            window_ptr.state.borrow_mut().ime_currently_visible = currently_visible;
            if currently_visible {
                // `android_input.on_screen_keyboard` user setting:
                // when false, the user opted out of the soft IME
                // (presumably uses a hardware keyboard). Skip the
                // auto-show on text-input focus but keep the
                // mirror seeded so if they later flip the setting
                // on and tap the keyboard button, the IME has
                // current state.
                if crate::ime::on_screen_keyboard_enabled() {
                    crate::ime::show_keyboard(&app, extra_window_id);
                } else {
                    log::info!(
                        "ime::reconcile auto-show suppressed by android_input.on_screen_keyboard=false"
                    );
                }
                crate::ime::notify_text_state(window_ptr);
            } else {
                // IME going hidden — clear cached selection on this
                // window so the next show-transition re-seeds cleanly.
                window_ptr.state.borrow_mut().last_pushed_selection = None;
                crate::ime::hide_keyboard(&app, extra_window_id);
            }
        }

        if !currently_visible {
            return;
        }

        // Probe handler kind + push updateSelection for touch-driven
        // cursor moves. Both are cheap: text_for_range(0..1) is a
        // single buffer lookup, selected_text_range is also O(1).
        // Only runs while IME is visible so background frames don't
        // pay this cost.
        self.tick_ime_target_and_selection(extra_window_id, window_ptr, &app);
    }

    /// Once per main-loop tick (while this window's IME is visible):
    /// detect target kind changes (terminal ↔ code editor pane
    /// swap) and push `updateSelection` whenever the editor's
    /// selection moved outside an IME-driven path. Solves two bugs:
    ///
    /// 1. Cross-pane composition contamination — Gboard's
    ///    composition state was sticky across pane swaps because
    ///    the InputConnection is shared. `restartInput` resets it.
    /// 2. Sticky cursor — after the user tap-moves the cursor
    ///    (which doesn't fire any IME event), Gboard didn't observe
    ///    the move; subsequent setComposingText extended the old
    ///    composition at the old position. updateSelection here
    ///    feeds Gboard the move so its next compose lands correctly.
    ///
    /// Also clears our composition tracking when the cursor leaves
    /// the marked range — a tap outside composition logically
    /// abandons it, even though the IME doesn't send finishComposing.
    fn tick_ime_target_and_selection(
        &self,
        extra_window_id: Option<u64>,
        window_ptr: &crate::window::AndroidWindowStatePtr,
        android_app: &android_activity::AndroidApp,
    ) {
        let current_kind: Option<crate::ime::ImeTargetKind>;
        let current_selection: Option<(usize, usize)>;
        let composition_anchor: Option<(usize, usize)>;
        let our_composition_active: bool;
        let handler_has_stale_mark: bool;
        let last_kind: Option<crate::ime::ImeTargetKind>;
        let last_selection: Option<(usize, usize)>;
        {
            let mut state = window_ptr.state.borrow_mut();
            last_kind = state.last_ime_target_kind;
            last_selection = state.last_pushed_selection;
            // Snapshot composition tracking before mut-borrowing
            // input_handler — once `handler` is alive, we can't
            // touch other fields of `state` immutably.
            our_composition_active = state.ime_composition_text.is_some();
            composition_anchor = state.ime_composition_start.and_then(|start| {
                state
                    .ime_composition_text
                    .as_ref()
                    .map(|t| (start, start + t.encode_utf16().count()))
            });
            let Some(handler) = state.input_handler.as_mut() else {
                return;
            };
            current_kind = Some(crate::ime::probe_target_kind(handler));
            current_selection = handler
                .selected_text_range(false)
                .map(|s| (s.range.start, s.range.end));
            // If the editor still has a marked range from a prior
            // composition session (user typed in editor, switched
            // away without committing, came back), the editor's
            // marked_text_ranges will still be set. Our next
            // setComposingText with range_utf16=None would then
            // accidentally replace those stale marks (editor.rs:
            // 23690 — "use marked_ranges if present"). Detect and
            // clean up while we have the handler at hand.
            handler_has_stale_mark = !our_composition_active && handler.marked_text_range().is_some();
            if handler_has_stale_mark {
                handler.unmark_text();
            }
        }
        if handler_has_stale_mark {
            log::info!("ime::tick cleared stale editor marked range");
        }

        let mut should_notify = false;

        // Detect target swap → restartInput with new EditorInfo.
        if current_kind != last_kind
            && let Some(kind) = current_kind
        {
            {
                let mut state = window_ptr.state.borrow_mut();
                state.ime_composition_start = None;
                state.ime_composition_text = None;
            }
            crate::ime::restart_input_for_kind(android_app, extra_window_id, kind);
            window_ptr.state.borrow_mut().last_ime_target_kind = Some(kind);
            should_notify = true;
        }

        // Detect selection move (touch-driven cursor click) →
        // updateSelection so the IME sees it.
        if current_selection != last_selection {
            window_ptr.state.borrow_mut().last_pushed_selection = current_selection;
            should_notify = true;

            // If cursor moved outside the active composition
            // (genuinely, not just the +N advance from typing in
            // composition), clear our tracking so the next
            // setComposingText starts a fresh composition at the
            // new cursor instead of trying to extend the old one
            // at the now-abandoned position.
            if let (Some((sel_start, sel_end)), Some((anchor_start, anchor_end))) =
                (current_selection, composition_anchor)
                && (sel_end < anchor_start || sel_start > anchor_end)
            {
                let mut state = window_ptr.state.borrow_mut();
                state.ime_composition_start = None;
                state.ime_composition_text = None;
            }
        }

        if should_notify {
            crate::ime::notify_text_state(window_ptr);
        }
    }

    /// Detect changes to `SOFT_KEYBOARD_VISIBLE` and force a window
    /// redraw so the pane keyboard button's `toggle_state` picks
    /// up the new value on the next paint. Runs every main-loop
    /// tick INDEPENDENTLY of input-handler visibility — the button
    /// is rendered on any focused pane (including those whose
    /// input handler is None, e.g. a terminal pane without active
    /// text-input), so gating this on `currently_visible` like
    /// `tick_ime_target_and_selection` would miss those cases.
    /// Detect `TRACKPAD_MODE_ENABLED` flips and tell every active
    /// window (primary + each registered extra) to show / hide its
    /// SurfaceControl cursor sprite. Each Activity owns its own
    /// cursor overlay bound to its own SurfaceView, so the
    /// broadcast has to fan out — without this, the user would
    /// have a visible cursor only on the window where trackpad
    /// mode was toggled, and any extra window (settings, picker,
    /// detached editor) would look broken when they navigated to
    /// it.
    fn tick_trackpad_mode_active(&self) {
        let current = crate::ime::trackpad_mode_enabled();
        let last = self.common.borrow().last_trackpad_mode_enabled;
        if current == last {
            return;
        }
        self.common.borrow_mut().last_trackpad_mode_enabled = current;
        crate::cursor::set_trackpad_mode_active(&self.android_app, None, current);
        let extra_ids: Vec<u64> = self
            .common
            .borrow()
            .extra_windows
            .keys()
            .copied()
            .collect();
        for id in extra_ids {
            crate::cursor::set_trackpad_mode_active(&self.android_app, Some(id), current);
        }
    }

    /// Push the current `EXTRAS_ROW_ENABLED` value to every live
    /// Activity (MainActivity + each extra) on edges. The Kotlin
    /// side inflates / removes its `ExtraKeysView` in response so
    /// the row appears / disappears without an app restart when
    /// the user toggles the setting.
    fn tick_extras_row_enabled(&self) {
        let current = crate::programming_extras_row_enabled();
        let last = self.common.borrow().last_extras_row_enabled;
        if last == Some(current) {
            return;
        }
        self.common.borrow_mut().last_extras_row_enabled = Some(current);
        if let Err(err) = call_activity_set_extras_row(&self.android_app, None, current) {
            log::warn!("set_extras_row_enabled(primary): {err:#}");
        }
        let extra_ids: Vec<u64> = self
            .common
            .borrow()
            .extra_windows
            .keys()
            .copied()
            .collect();
        for id in extra_ids {
            if let Err(err) =
                call_activity_set_extras_row(&self.android_app, Some(id), current)
            {
                log::warn!("set_extras_row_enabled(w={id}): {err:#}");
            }
        }
    }

    /// Push the user's `android_input.on_screen_keyboard` setting
    /// (mirrored in `ime::ON_SCREEN_KEYBOARD_ENABLED`) to every
    /// live Activity on edges. Each Activity stores the value in
    /// `softKeyboardEnabled` (an `ImeHost` interface property);
    /// `ImeHostView.onCheckIsTextEditor` reads it to gate Android's
    /// IME auto-show on focus.
    fn tick_soft_keyboard_setting(&self) {
        let current = crate::ime::on_screen_keyboard_enabled();
        let last = self.common.borrow().last_soft_keyboard_setting;
        if last == Some(current) {
            return;
        }
        self.common.borrow_mut().last_soft_keyboard_setting = Some(current);
        if let Err(err) = call_activity_set_soft_keyboard(&self.android_app, None, current) {
            log::warn!("set_soft_keyboard_enabled(primary): {err:#}");
        }
        let extra_ids: Vec<u64> = self
            .common
            .borrow()
            .extra_windows
            .keys()
            .copied()
            .collect();
        for id in extra_ids {
            if let Err(err) =
                call_activity_set_soft_keyboard(&self.android_app, Some(id), current)
            {
                log::warn!("set_soft_keyboard_enabled(w={id}): {err:#}");
            }
        }
    }

    fn tick_soft_keyboard_visibility(&self) {
        let current = crate::ime::soft_keyboard_visible();
        let last = self.common.borrow().last_soft_keyboard_visible;
        if current == last {
            return;
        }
        self.common.borrow_mut().last_soft_keyboard_visible = current;
        if let Some(window_ptr) = self.common.borrow().window.clone() {
            window_ptr
                .state
                .borrow_mut()
                .force_render_after_recovery = true;
            FRAME_PENDING.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Pull every queued `InputEvent` off android-activity's iterator and
    /// route translatable ones into the primary gpui window. Returning
    /// `InputStatus::Handled` for our own events lets android-activity stop
    /// propagating them up the system input stack.
    ///
    /// Extra-window inputs are NOT routed here — each `ExtraWindowActivity`
    /// has its own input pipeline (`OnTouchListener` + `OnKeyListener`) that
    /// JNIs into `multi_window` directly via `NativeBridge`. This loop only
    /// concerns events that GameActivity's native input queue receives,
    /// which is the primary surface alone.
    fn drain_input_events(&self) {
        use android_activity::InputStatus;
        use android_activity::input::InputEvent;

        let Some(window_ptr) = self.common.borrow().window.clone() else {
            return;
        };
        let Ok(mut iter) = self.android_app.input_events_iter() else {
            return;
        };
        let scale_factor = self.compute_scale_factor();
        loop {
            let read = iter.next(|event| match event {
                InputEvent::KeyEvent(key) => match crate::events::translate_key_event(key) {
                    Some(input) => {
                        window_ptr.handle_input(input);
                        InputStatus::Handled
                    }
                    None => InputStatus::Unhandled,
                },
                InputEvent::MotionEvent(motion) => {
                    let inputs = {
                        let mut state = window_ptr.state.borrow_mut();
                        crate::events::translate_motion_event(motion, scale_factor, &mut state)
                    };
                    if inputs.is_empty() {
                        InputStatus::Unhandled
                    } else {
                        for input in inputs {
                            window_ptr.handle_input(input);
                        }
                        InputStatus::Handled
                    }
                }
                _ => InputStatus::Unhandled,
            });
            if !read {
                break;
            }
        }
    }

    fn handle_main_event(&self, event: android_activity::MainEvent<'_>) {
        use android_activity::MainEvent;
        match event {
            MainEvent::InitWindow { .. } => {
                let window_ptr = self.common.borrow().window.clone();
                let Some(window_ptr) = window_ptr else {
                    log::warn!("MainEvent::InitWindow received before any window registered");
                    return;
                };
                let Some(native_window) = self.android_app.native_window() else {
                    log::warn!("MainEvent::InitWindow but native_window() returned None");
                    return;
                };
                let scale_factor = self.compute_scale_factor();
                if let Err(e) = window_ptr.attach_surface(native_window, scale_factor) {
                    log::error!("attach_surface failed: {e:#}");
                }
            }
            MainEvent::TerminateWindow { .. } => {
                if let Some(window_ptr) = self.common.borrow().window.clone() {
                    window_ptr.detach_surface();
                }
            }
            MainEvent::WindowResized { .. } => {
                let window_ptr = self.common.borrow().window.clone();
                let Some(window_ptr) = window_ptr else { return };
                let Some(native_window) = self.android_app.native_window() else { return };
                window_ptr.resize_surface(
                    native_window.width() as u32,
                    native_window.height() as u32,
                    self.compute_scale_factor(),
                );
            }
            MainEvent::ConfigChanged { .. } => {
                // Density may have changed (rotation, dock/scaling). Refresh
                // scale_factor and re-emit a resize so layout picks it up.
                let window_ptr = self.common.borrow().window.clone();
                let Some(window_ptr) = window_ptr else { return };
                let Some(native_window) = self.android_app.native_window() else { return };
                window_ptr.resize_surface(
                    native_window.width() as u32,
                    native_window.height() as u32,
                    self.compute_scale_factor(),
                );
                let new_appearance = appearance_from_config(&self.android_app);
                self.common.borrow_mut().appearance = new_appearance;
                window_ptr.set_appearance(new_appearance);
            }
            MainEvent::RedrawNeeded { .. } => {
                if let Some(window_ptr) = self.common.borrow().window.clone() {
                    window_ptr.refresh();
                }
            }
            MainEvent::Destroy => {
                self.common.borrow_mut().running = false;
            }
            _ => {}
        }
    }
}

impl Platform for AndroidPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.common.borrow().background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.common.borrow().foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.common.borrow().text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        log::info!("AndroidPlatform::run: invoking on_finish_launching");
        on_finish_launching();
        log::info!("AndroidPlatform::run: entering event loop");

        // Hook AChoreographer for vsync-aligned rendering. The callback
        // is delivered on this thread's looper as part of the same
        // event stream android-activity polls, so vsync arrivals
        // unblock `poll_events` naturally — we don't need to drive
        // refresh from a tight timeout.
        install_choreographer_callback();

        while self.common.borrow().running {
            // 100ms is the upper bound (idle / unfocused). Any of:
            //   - input event
            //   - vsync (`frame_callback` runs as a looper task, sets
            //     FRAME_PENDING)
            //   - an enqueued waker (background-thread runnable)
            //   - main-thread event from android-activity
            // returns earlier. With the Choreographer driving us, this
            // loop ticks at the panel's refresh rate (60Hz / 90Hz /
            // 120Hz / etc.) when active and falls to ~10Hz idle.
            self.android_app.poll_events(
                Some(std::time::Duration::from_millis(100)),
                |event| match event {
                    android_activity::PollEvent::Wake => {}
                    android_activity::PollEvent::Timeout => {}
                    android_activity::PollEvent::Main(main_event) => {
                        log::trace!("MainEvent: {main_event:?}");
                        self.handle_main_event(main_event);
                    }
                    _ => {}
                },
            );

            // Drain main-thread runnables enqueued from background threads. The
            // AndroidAppWaker wakes poll_events above when there's work; we drain
            // each tick regardless to catch anything between waker and poll.
            let receiver = self.common.borrow().main_receiver.clone();
            for runnable in receiver.try_iter() {
                if let Ok(runnable) = runnable {
                    runnable.run();
                }
            }

            // Drain input events into the active window. We call this every
            // tick rather than only on Wake — android-activity gates input
            // delivery on this iterator being polled, so missing a tick can
            // stall touch.
            self.drain_input_events();

            // Drain JNI-side extra-window lifecycle / touch events.
            self.drain_extra_window_events();

            // Drain JNI-side captured-pointer (trackpad) events. Active
            // only while `MainActivity` has pointer capture; otherwise
            // the channel sits idle.
            self.drain_captured_pointer_events();

            // Drain JNI-side IME events (commit/composition/key/delete
            // delivered by `ZdroidInputConnection` on Android's UI
            // thread). Dispatches into the primary window's
            // `PlatformInputHandler` / `handle_input`.
            self.drain_ime_events();

            // Reconcile IME visibility against the primary window's
            // `input_handler` presence. Edge-triggered at frame
            // boundaries — gpui's take/set oscillation within each
            // paint can't leak through because we're sampling between
            // ticks, not inside set/take callbacks.
            self.reconcile_ime_visibility();
            self.tick_soft_keyboard_visibility();
            self.tick_trackpad_mode_active();
            self.tick_extras_row_enabled();
            self.tick_soft_keyboard_setting();

            // Refresh on vsync (FRAME_PENDING set by Choreographer
            // callback) or after main-thread events that may have
            // changed state. gpui's window.refresh() short-circuits
            // when nothing is dirty, so calling on every iteration is
            // cheap; gating on FRAME_PENDING saves the dirty-bit check
            // when we know nothing's happened.
            if FRAME_PENDING.swap(false, Ordering::AcqRel) {
                let (primary, extras) = {
                    let common = self.common.borrow();
                    (
                        common.window.clone(),
                        common.extra_windows.values().cloned().collect::<Vec<_>>(),
                    )
                };
                crate::frame_timing::paint_started();
                if let Some(window_ptr) = primary {
                    window_ptr.refresh();
                }
                for window_ptr in extras {
                    window_ptr.refresh();
                }
                crate::frame_timing::paint_finished();
                // First successful paint cycle => let the SplashScreen
                // poller dismiss its window. Idempotent atomic swap on
                // the inside; safe to call every frame.
                crate::splash::mark_zed_ready();
            }
        }

        log::info!("AndroidPlatform::run: exiting event loop");
        let quit = self.common.borrow_mut().callbacks.quit.take();
        if let Some(mut fun) = quit {
            fun();
        }
    }

    fn quit(&self) {
        self.common.borrow_mut().running = false;
        let quit = self.common.borrow_mut().callbacks.quit.take();
        if let Some(mut fun) = quit {
            fun();
        }
        // gpui's quit hook only stops the run loop. On Android the
        // hosting GameActivity stays alive (its JVM-side thread keeps
        // running independently of `android_main` returning), so the
        // user-visible result of a "Quit" menu click would be: editor
        // stops responding to gestures but the window doesn't dismiss.
        // Exit the process so `MainActivity.onDestroy` runs (it does
        // its own `Process.killProcess` fallback + cursor / splash
        // teardown) and the OS returns the user to the launcher.
        std::process::exit(0);
    }

    fn restart(&self, _binary_path: Option<PathBuf>) {}
    fn activate(&self, _ignoring_other_apps: bool) {}
    fn hide(&self) {}
    fn hide_other_apps(&self) {}
    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![Rc::new(AndroidDisplay::new())]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(AndroidDisplay::new()))
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.common.borrow().active_window
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let primary_present = self.common.borrow().window.is_some();
        if primary_present {
            return self.open_extra_window(handle, options);
        }

        // gpui's `Window::new` calls `platform_window.sprite_atlas()` immediately,
        // so the renderer (and therefore atlas) must already be live by the time
        // we return. On Android we have no surface until `MainEvent::InitWindow`
        // fires, so block here pumping the Android event loop until a native
        // window is available, then attach inline.
        //
        // poll_events is reentrant-safe — `on_finish_launching` runs before our
        // outer poll loop in `run()`, so this is the only `poll_events` call
        // on the stack right now.
        while self.android_app.native_window().is_none() {
            if !self.common.borrow().running {
                return Err(anyhow::anyhow!(
                    "open_window: app destroyed before surface attached"
                ));
            }
            self.android_app.poll_events(
                Some(std::time::Duration::from_millis(100)),
                |event| {
                    if let android_activity::PollEvent::Main(main_event) = event {
                        log::trace!("MainEvent during open_window block: {main_event:?}");
                        self.handle_main_event(main_event);
                    }
                },
            );

            // NOTE: do not drain main_receiver here. open_window runs inside
            // gpui's `cx.update` borrow guard; a runnable that calls
            // `cx.update(...)` re-enters the borrow and panics ("RefCell
            // already borrowed"). The outer event loop drains the queue on
            // the next tick, so queued work picks up fine.
        }

        let appearance = self.common.borrow().appearance;
        let gpu_context = self.common.borrow().gpu_context.clone();
        let window =
            AndroidWindow::new(handle, options, gpu_context, appearance, self.android_app.clone());

        let native_window = self.android_app.native_window().ok_or_else(|| {
            anyhow::anyhow!("open_window: native_window vanished between poll and attach")
        })?;
        let scale_factor = self.compute_scale_factor();
        window.ptr().attach_surface(native_window, scale_factor)?;

        self.common.borrow_mut().window = Some(window.ptr());
        self.common.borrow_mut().active_window = Some(handle);
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        self.common.borrow().appearance
    }

    fn open_url(&self, url: &str) {
        if let Err(err) = jni_open_url(&self.android_app, url) {
            log::warn!("AndroidPlatform::open_url({url:?}) failed: {err:#}");
        }
    }
    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.common.borrow_mut().callbacks.open_urls = Some(callback);
    }
    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        // Fire ACTION_OPEN_DOCUMENT_TREE via MainActivity. The result
        // arrives async through the JNI callback in `saf.rs`.
        log::info!("AndroidPlatform::prompt_for_paths invoked");
        let (tx, rx) = oneshot::channel();
        crate::saf::pick_folder(&self.android_app, tx);
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        // ACTION_CREATE_DOCUMENT — the system picker decides the
        // directory; we only suggest a name. Result arrives via the
        // same JNI callback.
        log::info!(
            "AndroidPlatform::prompt_for_new_path invoked (suggested={:?})",
            suggested_name
        );
        let (tx, rx) = oneshot::channel();
        crate::saf::pick_new_path(&self.android_app, suggested_name, tx);
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }
    fn reveal_path(&self, _path: &Path) {}
    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.common.borrow_mut().callbacks.quit = Some(callback);
    }
    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.common.borrow_mut().callbacks.reopen = Some(callback);
    }
    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {
        // Android exposes no system sleep/wake signal to apps; process
        // lifecycle (onResume etc.) is a different concept and already
        // flows through the app-lifecycle path. Drop the callback.
    }
    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}
    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}
    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.common.borrow_mut().callbacks.app_menu_action = Some(callback);
    }
    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.common.borrow_mut().callbacks.will_open_app_menu = Some(callback);
    }
    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.common.borrow_mut().callbacks.validate_app_menu_command = Some(callback);
    }

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }
    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn compositor_name(&self) -> &'static str {
        "Android"
    }
    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow::anyhow!("app_path is not yet implemented on Android"))
    }
    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow::anyhow!(
            "auxiliary executables are not available on Android"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        crate::cursor::set_pointer_icon(&self.android_app, style);
    }
    fn hide_cursor_until_mouse_moves(&self) {}
    fn is_cursor_visible(&self) -> bool {
        true
    }
    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        crate::clipboard::read(&self.android_app)
    }
    fn write_to_clipboard(&self, item: ClipboardItem) {
        crate::clipboard::write(&self.android_app, item);
    }

    fn write_credentials(
        &self,
        _url: &str,
        _username: &str,
        _password: &[u8],
    ) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "credential storage not yet wired on Android"
        )))
    }
    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }
    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "credential storage not yet wired on Android"
        )))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(AndroidKeyboardLayout)
    }
    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }
    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.common.borrow_mut().callbacks.keyboard_layout_change = Some(callback);
    }
}

/// Invoke `Activity.setProgrammingExtrasRowEnabled(enabled)` on the
/// primary or one extra Activity. Used by
/// `AndroidPlatform::tick_extras_row_enabled` to push the user's
/// `android_input.programming_extras_row` setting into Kotlin so it
/// can inflate / remove the `ExtraKeysView` without an app restart.
fn call_activity_set_extras_row(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    enabled: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use jni::{JavaVM, objects::JObject};

    let vm_ptr = android_app.vm_as_ptr();
    let activity_ptr = android_app.activity_as_ptr();
    if vm_ptr.is_null() || activity_ptr.is_null() {
        anyhow::bail!("AndroidApp vm/activity pointer is null");
    }
    let vm = unsafe { JavaVM::from_raw(vm_ptr as _) }.context("JavaVM::from_raw")?;
    let mut env = vm.attach_current_thread().context("attach_current_thread")?;
    let args = [enabled.into()];
    match extra_window_id {
        Some(id) => {
            let Some(activity_ref) = crate::multi_window::extra_activity_for(id) else {
                return Ok(());
            };
            env.call_method(
                activity_ref.as_obj(),
                "setProgrammingExtrasRowEnabled",
                "(Z)V",
                &args,
            )
            .context("call ExtraWindowActivity.setProgrammingExtrasRowEnabled")?;
        }
        None => {
            let activity = unsafe { JObject::from_raw(activity_ptr as _) };
            env.call_method(
                &activity,
                "setProgrammingExtrasRowEnabled",
                "(Z)V",
                &args,
            )
            .context("call MainActivity.setProgrammingExtrasRowEnabled")?;
        }
    }
    Ok(())
}

/// Invoke `Activity.setSoftKeyboardEnabled(enabled)` on the primary
/// or one extra Activity. Used by `tick_soft_keyboard_setting` to
/// push the user's `android_input.on_screen_keyboard` setting into
/// Kotlin so `ImeHostView.onCheckIsTextEditor` can gate Android's
/// IME auto-show. Routes through the held Activity object (no
/// `find_class` involved) because JNI's `find_class` against app
/// classes is unreliable from any thread that wasn't spawned by
/// the JVM with the app's class loader — and the android_main
/// thread isn't one of those (it's a native thread spawned by
/// android-activity's glue).
fn call_activity_set_soft_keyboard(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    enabled: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use jni::{JavaVM, objects::JObject};

    let vm_ptr = android_app.vm_as_ptr();
    let activity_ptr = android_app.activity_as_ptr();
    if vm_ptr.is_null() || activity_ptr.is_null() {
        anyhow::bail!("AndroidApp vm/activity pointer is null");
    }
    let vm = unsafe { JavaVM::from_raw(vm_ptr as _) }.context("JavaVM::from_raw")?;
    let mut env = vm.attach_current_thread().context("attach_current_thread")?;
    let args = [enabled.into()];
    match extra_window_id {
        Some(id) => {
            let Some(activity_ref) = crate::multi_window::extra_activity_for(id) else {
                return Ok(());
            };
            env.call_method(
                activity_ref.as_obj(),
                "setSoftKeyboardEnabled",
                "(Z)V",
                &args,
            )
            .context("call ExtraWindowActivity.setSoftKeyboardEnabled")?;
        }
        None => {
            let activity = unsafe { JObject::from_raw(activity_ptr as _) };
            env.call_method(
                &activity,
                "setSoftKeyboardEnabled",
                "(Z)V",
                &args,
            )
            .context("call MainActivity.setSoftKeyboardEnabled")?;
        }
    }
    Ok(())
}
