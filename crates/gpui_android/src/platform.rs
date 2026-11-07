use std::{
    cell::{Cell, OnceCell, RefCell},
    ffi::{OsString, c_void},
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use android_activity::{AndroidApp, AndroidAppWaker};
use anyhow::Result;
use futures::channel::{mpsc, oneshot};
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, AppLifecyclePhase, BackgroundExecutor, Bounds,
    ClipboardItem, CursorStyle, DummyKeyboardMapper, ForegroundExecutor, GestureTuning, Keymap,
    Menu, MenuItem, PathPromptOptions, Pixels, Platform, PlatformDisplay, PlatformGestures,
    PlatformInput, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, PriorityQueueReceiver, RunnableVariant, ScrollPhysics, Task, ThermalState,
    TouchPhase, WindowAppearance, WindowParams, px,
};
use gpui_wgpu::GpuContext;
use ndk::configuration::UiModeNight;

use crate::dispatcher::AndroidDispatcher;
use crate::display::AndroidDisplay;
use crate::keyboard::AndroidKeyboardLayout;
use crate::window::{AndroidWindow, AndroidWindowStatePtr};

// AChoreographer, the NDK's vsync scheduler: Java's would need a JNI hop on
// every vsync. The `ndk` crate doesn't wrap it.
#[link(name = "android")]
unsafe extern "C" {
    fn AChoreographer_getInstance() -> *mut c_void;
    fn AChoreographer_postFrameCallback64(
        choreographer: *mut c_void,
        callback: ChoreographerFrameCallback,
        data: *mut c_void,
    );
}

type ChoreographerFrameCallback = unsafe extern "C" fn(frame_time_nanos: i64, data: *mut c_void);

/// Set by the Choreographer callback (called on the main thread's looper
/// at the vsync after a frame was requested, so no synchronization beyond
/// the atomic). Checked at the end of each event-loop tick to decide
/// whether to call window.refresh().
static FRAME_PENDING: AtomicBool = AtomicBool::new(false);

/// Whether a Choreographer callback is posted and hasn't fired yet.
static FRAME_CALLBACK_POSTED: AtomicBool = AtomicBool::new(false);

/// Live guards from `prevent_idle_sleep`.
static IDLE_SLEEP_GUARDS: AtomicUsize = AtomicUsize::new(0);

/// Wakes the run loop from Java threads. JNI callbacks queue input and
/// window events, or set flags, that the loop only reads on its next tick,
/// and an idle loop otherwise sleeps until something wakes it.
static RUN_LOOP_WAKER: Mutex<Option<AndroidAppWaker>> = Mutex::new(None);

static PATH_PROMPTS: Mutex<Option<mpsc::UnboundedSender<PathPrompt>>> = Mutex::new(None);

/// A path prompt Zed sent to the platform, for the app to show. Dropping
/// `answer` cancels it.
pub struct PathPrompt {
    /// Whether the answer is a path to write to rather than one to open.
    pub creating_path: bool,
    pub suggested_name: Option<String>,
    pub answer: oneshot::Sender<Option<Vec<PathBuf>>>,
}

/// The path prompts Zed sends to the platform. They come through a channel
/// because they arrive while the caller holds the `App`, which showing a
/// picker needs.
pub fn path_prompts() -> mpsc::UnboundedReceiver<PathPrompt> {
    let (sender, receiver) = mpsc::unbounded();
    *PATH_PROMPTS.lock().unwrap_or_else(PoisonError::into_inner) = Some(sender);
    receiver
}

fn request_path_prompt(
    creating_path: bool,
    suggested_name: Option<String>,
) -> oneshot::Receiver<Option<Vec<PathBuf>>> {
    let (answer, paths) = oneshot::channel();
    let prompt = PathPrompt {
        creating_path,
        suggested_name,
        answer,
    };
    let sent = PATH_PROMPTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|prompts| prompts.unbounded_send(prompt).is_ok());
    if !sent {
        log::error!("no receiver for a path prompt, so it's cancelled");
    }
    paths
}

thread_local! {
    /// When a window last presented, if no frame has started since.
    static LAST_PRESENT_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// A frame that starts within this long of the last present continues a
/// run of frames; after a longer gap it follows an idle pause.
const FRAME_RUN_GAP: Duration = Duration::from_millis(50);

pub(crate) fn wake_run_loop() {
    if let Some(waker) = RUN_LOOP_WAKER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
    {
        waker.wake();
    }
}

unsafe extern "C" fn frame_callback(_frame_time_nanos: i64, _data: *mut c_void) {
    FRAME_CALLBACK_POSTED.store(false, Ordering::Release);
    FRAME_PENDING.store(true, Ordering::Release);
}

/// Asks for a refresh on the next vsync. Called through each window's frame
/// waker, which gpui invokes while a window is dirty or still animating, so
/// an idle editor stops waking at the panel's refresh rate. gpui's web and
/// Wayland backends drive their frame loops the same way.
pub(crate) fn request_frame_callback() {
    // A vsync already arrived and the run loop refreshes before it polls
    // again; gpui asks for another frame afterwards if it still has work.
    if FRAME_PENDING.load(Ordering::Acquire) {
        return;
    }
    if FRAME_CALLBACK_POSTED.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe {
        let choreographer = AChoreographer_getInstance();
        if choreographer.is_null() {
            // No vsync source: refresh on the run loop's next tick instead.
            FRAME_CALLBACK_POSTED.store(false, Ordering::Release);
            FRAME_PENDING.store(true, Ordering::Release);
            wake_run_loop();
            return;
        }
        AChoreographer_postFrameCallback64(choreographer, frame_callback, std::ptr::null_mut());
    }
}

/// Called whenever a window presents. While frames that present run back
/// to back, the run loop asks for the next vsync before it paints. That
/// also carries gpui's own rule for fast input: it keeps presenting for a
/// second after input that changes something arrives at 60 events a
/// second or more, so the panel stays at its high refresh rate, and each
/// of those presents asks for the next frame.
pub(crate) fn frame_presented() {
    LAST_PRESENT_AT.set(Some(Instant::now()));
}

/// Dispatches the next step of a window's trackpad glide, and asks for another
/// vsync while it runs. A hidden window's glide stops.
fn advance_trackpad_glide(window_ptr: &crate::window::AndroidWindowStatePtr) {
    let visible = window_ptr.is_visible();
    let (step, gliding) = {
        let mut state = window_ptr.state.borrow_mut();
        if !visible {
            state.trackpad_scroll.stop_glide();
            return;
        }
        let step = state.trackpad_scroll.tick();
        (step, state.trackpad_scroll.is_gliding())
    };
    if let Some(event) = step {
        window_ptr.handle_input(PlatformInput::Scroll(event));
    }
    if gliding {
        request_frame_callback();
    }
}

/// Requests the first frame on the calling thread, which must have a Looper
/// attached (android-activity's main thread does).
fn install_choreographer_callback() {
    if unsafe { AChoreographer_getInstance() }.is_null() {
        log::warn!(
            "AChoreographer_getInstance returned null; vsync sync \
             disabled, refreshing on every run loop tick"
        );
    } else {
        log::info!("AndroidPlatform: Choreographer frame callbacks enabled");
    }
    request_frame_callback();
}

#[derive(Default)]
pub(crate) struct PlatformHandlers {
    pub(crate) open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    pub(crate) quit: Option<Box<dyn FnMut() -> bool>>,
    pub(crate) app_lifecycle: Option<Box<dyn FnMut(AppLifecyclePhase)>>,
    pub(crate) memory_warning: Option<Box<dyn FnMut()>>,
    pub(crate) thermal_state_change: Option<Box<dyn FnMut()>>,
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
    pub(crate) extra_event_rx:
        Option<futures::channel::mpsc::UnboundedReceiver<crate::multi_window::ExtraWindowEvent>>,
    /// Receiver side of the JNI → game-thread channel for IME events.
    /// Each event carries the originating window id (0 = primary
    /// `MainActivity`, otherwise the `ExtraWindowActivity` window id
    /// gpui assigned). Drained each iteration of the platform run
    /// loop; `crate::ime::drain_ime_events` routes per-id into the
    /// right window's `PlatformInputHandler`.
    pub(crate) ime_event_rx:
        Option<futures::channel::mpsc::UnboundedReceiver<(u64, crate::ime::ImeEvent)>>,
    /// Tracks whether the soft keyboard was visible last tick.
    /// When this disagrees with the atomic Kotlin pushes via
    /// `nativeSetSoftKeyboardVisible`, we force a `window.refresh()`
    /// so the status bar keyboard button re-renders with its updated
    /// `toggle_state`. Without this, the atomic flips on each
    /// toggle but the UI keeps showing stale state until some
    /// other event triggers a paint — the user-visible bug where
    /// the button needs two taps to "respond".
    pub(crate) last_soft_keyboard_visible: bool,
    /// The `PHYSICAL_KEYBOARD_BLOCKS_IME` value last acted on.
    pub(crate) last_physical_keyboard_blocks_ime: bool,
    /// The settings last pushed to the Activities; see `tick_window_settings`.
    pub(crate) last_extras_row_enabled: Option<bool>,
    pub(crate) last_soft_keyboard_setting: Option<bool>,
    /// Main window events that arrived while an extra window was opening,
    /// for the run loop to apply: gpui can't take callbacks then.
    pub(crate) deferred_primary_resync: bool,
    pub(crate) deferred_primary_focus: Option<bool>,
    pub(crate) deferred_lifecycle: Vec<AppLifecyclePhase>,
    pub(crate) deferred_memory_warning: bool,
    /// Whether the main Activity keeps the screen on; see `prevent_idle_sleep`.
    pub(crate) keep_screen_on: bool,
    pub(crate) drew_first_frame: bool,
    pub(crate) running: bool,
    /// Set by `quit()` so `run()` exits the process once the event loop ends.
    pub(crate) exit_requested: bool,
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
        // first-load on a high-end device) and surfaces NotoSansSymbols2,
        // NotoColorEmoji, DroidSansMono, plus the full Noto family for
        // CJK / Cyrillic / Arabic / etc. Without this fallback, prompts
        // and statuslines that use powerline glyphs (`❯` U+276F),
        // dingbats (`✻` U+273B), or any non-Latin codepoint render as
        // tofu boxes because Lilex + IBM Plex Sans only cover basic
        // Latin / extended Latin.
        let cosmic = gpui_wgpu::CosmicTextSystem::new_without_system_fonts("Lilex");
        cosmic.load_fonts_dir("/system/fonts");
        // The folders fontconfig reads on desktop Linux, where desktop Zed
        // finds the user's fonts: the home folder's, and the runtime's for
        // fonts installed with apt. The terminal is drawn here, so a font
        // installed in the runtime only reaches it this way.
        let runtime = zdroid_runtime::Runtime::new(&zdroid_runtime::data_path());
        let rootfs = runtime.rootfs_dir();
        let started = std::time::Instant::now();
        for dir in [
            runtime.workspace_root().join(".local/share/fonts"),
            rootfs.join("usr/share/fonts"),
            rootfs.join("usr/local/share/fonts"),
        ] {
            if dir.is_dir() {
                cosmic.load_fonts_dir(&dir);
            }
        }
        log::info!(
            "fonts: loaded the home folder's and the runtime's fonts in {:?}",
            started.elapsed()
        );
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
            ime_event_rx: Some(crate::ime::init_event_channel()),
            last_soft_keyboard_visible: false,
            last_physical_keyboard_blocks_ime: false,
            last_extras_row_enabled: None,
            last_soft_keyboard_setting: None,
            deferred_primary_resync: false,
            deferred_primary_focus: None,
            deferred_lifecycle: Vec::new(),
            deferred_memory_warning: false,
            keep_screen_on: false,
            drew_first_frame: false,
            running: true,
            exit_requested: false,
        }
    }
}

fn appearance_from_config(android_app: &AndroidApp) -> WindowAppearance {
    match android_app.config().ui_mode_night() {
        UiModeNight::Yes => WindowAppearance::Dark,
        _ => WindowAppearance::Light,
    }
}

fn call_activity_with_path(android_app: &AndroidApp, method: &str, path: &Path) {
    let result = crate::jni_env::with_activity(android_app, |env, activity| {
        let path = env.new_string(path.to_string_lossy())?;
        env.call_method(activity, method, "(Ljava/lang/String;)V", &[(&path).into()])?;
        Ok(())
    });
    if let Err(error) = result {
        log::warn!("AndroidPlatform: MainActivity.{method}({path:?}) failed: {error:#}");
    }
}

/// Dispatch `MainActivity.openUrl(url: String)` via JNI. Backs
/// `AndroidPlatform::open_url`; the upstream gpui trait stub was empty so
/// `cx.open_url(...)` was a silent no-op on Android. The Kotlin side wraps
/// `startActivity(Intent.ACTION_VIEW, Uri.parse(url))` and swallows
/// ActivityNotFoundException.
fn jni_open_url(android_app: &AndroidApp, url: &str) -> Result<()> {
    use anyhow::Context;

    crate::jni_env::with_activity(android_app, |env, activity| {
        let url_jstring = env
            .new_string(url)
            .context("alloc JString for open_url argument")?;
        env.call_method(
            activity,
            "openUrl",
            "(Ljava/lang/String;)V",
            &[(&url_jstring).into()],
        )
        .context("MainActivity.openUrl")?;
        Ok(())
    })
}

/// Gesture feel for gpui's touch recognizer, from Android's
/// `ViewConfiguration`.
struct AndroidGestures {
    tuning: GestureTuning,
}

impl PlatformGestures for AndroidGestures {
    fn tuning(&self) -> GestureTuning {
        self.tuning
    }
}

impl AndroidGestures {
    fn new(android_app: &AndroidApp) -> Self {
        // AOSP's `ViewConfiguration` defaults, for when JNI fails. The
        // double-tap slop stays gpui's: Android's 100 dp would merge taps
        // on neighbouring words into a double tap.
        let mut tuning = GestureTuning {
            touch_slop: px(8.0),
            multi_tap_interval: Duration::from_millis(300),
            long_press_duration: Duration::from_millis(400),
            min_fling_velocity: 50.0,
            scroll_physics: ScrollPhysics::android(),
            ..GestureTuning::default()
        };
        match read_gesture_configuration(android_app) {
            Ok([touch_slop, min_fling_velocity, double_tap_ms, long_press_ms]) => {
                tuning.touch_slop = px(touch_slop);
                tuning.min_fling_velocity = min_fling_velocity;
                tuning.multi_tap_interval = Duration::from_millis(double_tap_ms as u64);
                tuning.long_press_duration = Duration::from_millis(long_press_ms as u64);
            }
            Err(error) => log::error!("gestures: reading ViewConfiguration failed: {error:#}"),
        }
        log::info!("gestures: {tuning:?}");
        Self { tuning }
    }
}

/// `MainActivity.gestureConfiguration`'s values.
fn read_gesture_configuration(android_app: &AndroidApp) -> Result<[f32; 4]> {
    crate::jni_env::with_activity(android_app, |env, activity| {
        let values = env
            .call_method(activity, "gestureConfiguration", "()[F", &[])?
            .l()?;
        let mut configuration = [0.0; 4];
        env.get_float_array_region(
            jni::objects::JFloatArray::from(values),
            0,
            &mut configuration,
        )?;
        Ok(configuration)
    })
}

/// Touch moves held back within one input drain, the latest for each
/// touch. The run loop drains input once per tick, so after a slow frame
/// several moves arrive at once, and gpui, which measures fling speed from
/// when touches arrive, would read them as a very fast finger.
#[derive(Default)]
struct PendingTouchMoves {
    moves: Vec<(AndroidWindowStatePtr, gpui::TouchEvent)>,
}

impl PendingTouchMoves {
    /// Dispatches `input` to `window`, but holds back a touch move until
    /// [`Self::flush`] or the next other input.
    fn dispatch(&mut self, window: &AndroidWindowStatePtr, input: PlatformInput) {
        match input {
            PlatformInput::Touch(touch) if touch.phase == TouchPhase::Moved => {
                let pending = self.moves.iter_mut().find(|(pending_window, pending)| {
                    Rc::ptr_eq(&pending_window.state, &window.state) && pending.id == touch.id
                });
                match pending {
                    Some((_, pending)) => *pending = touch,
                    None => self.moves.push((window.clone(), touch)),
                }
            }
            input => {
                self.flush();
                window.handle_input(input);
            }
        }
    }

    fn flush(&mut self) {
        for (window, touch) in self.moves.drain(..) {
            window.handle_input(PlatformInput::Touch(touch));
        }
    }
}

pub struct AndroidPlatform {
    pub(crate) common: RefCell<AndroidCommon>,
    pub(crate) android_app: AndroidApp,
    gestures: OnceCell<Rc<AndroidGestures>>,
}

impl AndroidPlatform {
    pub fn new(android_app: AndroidApp, _headless: bool) -> Self {
        *RUN_LOOP_WAKER
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(android_app.create_waker());
        // GameActivity only keeps the axes an app asks for, and these are a mouse
        // wheel's.
        android_app.enable_motion_axis(android_activity::input::Axis::Vscroll);
        android_app.enable_motion_axis(android_activity::input::Axis::Hscroll);
        let common = AndroidCommon::new(&android_app);
        Self {
            common: RefCell::new(common),
            android_app,
            gestures: OnceCell::new(),
        }
    }

    fn android_gestures(&self) -> Rc<AndroidGestures> {
        self.gestures
            .get_or_init(|| Rc::new(AndroidGestures::new(&self.android_app)))
            .clone()
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
        let screen_w = nw.width();
        let screen_h = nw.height();
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
    /// Android reports density in dpi where 160 dpi = 1.0x, so 336 dpi
    /// becomes ~2.1x. Falls back to 1.0 if the density isn't yet
    /// reported (e.g. very early in startup before the first config arrives).
    fn compute_scale_factor(&self) -> f32 {
        self.config_scale_factor().unwrap_or(1.0)
    }

    fn config_scale_factor(&self) -> Option<f32> {
        match self.android_app.config().density() {
            Some(dpi) if dpi > 0 => Some((dpi as f32 / 160.0).max(1.0)),
            _ => None,
        }
    }

    /// Branch of [`open_window`](Self::open_window) for the second-and-beyond
    /// `cx.open_window` call. Launches an `ExtraWindowActivity` via JNI Intent,
    /// waits for its `surfaceCreated` callback, then wraps the
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

        let native_window = crate::multi_window::create_extra_window_blocking(
            &self.android_app,
            window_id,
            launch_bounds,
            |timeout| {
                self.android_app.poll_events(Some(timeout), |event| {
                    if let android_activity::PollEvent::Main(main_event) = event {
                        self.handle_main_event_while_opening_window(main_event);
                    }
                });
                self.common.borrow().running
            },
        );
        // The waits above consumed wakes meant for the run loop (runnables,
        // JNI events, vsync, input), which won't be sent again.
        wake_run_loop();
        let native_window = match native_window {
            Ok(nw) => nw,
            Err(err) => {
                crate::multi_window::unmark_window_registered(window_id);
                return Err(err);
            }
        };

        let appearance = self.common.borrow().appearance;
        let gpu_context = self.common.borrow().gpu_context.clone();
        let window = AndroidWindow::new(
            handle,
            options,
            gpu_context,
            appearance,
            self.android_app.clone(),
            self.android_gestures().tuning,
        );
        window.ptr().state.borrow_mut().extra_window_id = Some(window_id);
        if let Err(err) = window.ptr().attach_surface(native_window, scale_factor) {
            crate::multi_window::unmark_window_registered(window_id);
            crate::multi_window::finish_extra_activity(&self.android_app, window_id);
            return Err(err);
        }
        // Before gpui reads the window's size: its Activity may have
        // reported its insets while this waited for the surface.
        window.ptr().set_insets(crate::insets::reported(window_id));

        {
            let mut common = self.common.borrow_mut();
            common.extra_windows.insert(window_id, window.ptr());
            common.active_window = Some(handle);
        }
        self.push_settings_to_new_window(window_id);
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
        // here would no-op against an empty callback slot. The Activity's
        // focus may have changed by then, so send the current state.
        let executor = self.common.borrow().foreground_executor.clone();
        let window_ptr = window.ptr();
        executor
            .spawn(async move {
                let active = window_ptr.state.borrow().active;
                window_ptr.notify_active_status_change(active);
            })
            .detach();
        Ok(Box::new(window))
    }

    /// The Activities only hear of these when they change, so a window opened
    /// since then gets their current values here. Without the keyboard setting,
    /// its Activity refuses the keyboard's text connection, and typing falls
    /// back to plain key presses.
    fn push_settings_to_new_window(&self, window_id: u64) {
        let (keep_screen_on, soft_keyboard_setting) = {
            let common = self.common.borrow();
            (common.keep_screen_on, common.last_soft_keyboard_setting)
        };
        let window_id = Some(window_id);
        let push = |method: &str, signature: &str, argument: jni::objects::JValue<'_, '_>| {
            let result = crate::jni_env::call_window_method(
                &self.android_app,
                window_id,
                method,
                signature,
                &[argument],
            );
            if let Err(error) = result {
                log::warn!("{method}(w={window_id:?}): {error:#}");
            }
        };
        if keep_screen_on {
            push("setKeepScreenOn", "(Z)V", true.into());
        }
        if let Some(enabled) = soft_keyboard_setting {
            push("setSoftKeyboardEnabled", "(Z)V", enabled.into());
        }
        if let Some(icon_type) = crate::cursor::last_pushed_icon_type() {
            push("setCursorStyle", "(I)V", icon_type.into());
        }
    }

    /// Drops extra windows that gpui has closed. gpui drops an extra
    /// window's `AndroidWindow` when it closes it, which marks the window
    /// closed, but the platform still holds it. Refreshing it would make
    /// gpui's frame handler fail with "window not found", log that, and
    /// ask for another frame, every frame.
    fn reap_closed_extra_windows(&self) {
        let mut common = self.common.borrow_mut();
        let closed: Vec<u64> = common
            .extra_windows
            .iter()
            .filter(|(_, window)| window.state.borrow().closed)
            .map(|(window_id, _)| *window_id)
            .collect();
        if closed.is_empty() {
            return;
        }
        for window_id in &closed {
            common.extra_windows.remove(window_id);
            crate::ime::set_text_input_focused(*window_id, false);
        }
        if common
            .active_window
            .is_some_and(|handle| closed.contains(&handle.window_id().as_u64()))
        {
            common.active_window = common
                .window
                .as_ref()
                .map(|primary| primary.state.borrow().handle);
        }
    }

    /// Hands each window the insets its Activity last reported.
    fn apply_window_insets(&self) {
        if !crate::insets::take_changed() {
            return;
        }
        let (primary, extras) = {
            let common = self.common.borrow();
            (
                common.window.clone(),
                common
                    .extra_windows
                    .iter()
                    .map(|(window_id, window)| (*window_id, window.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        if let Some(primary) = primary {
            primary.set_insets(crate::insets::reported(crate::insets::PRIMARY_WINDOW_ID));
        }
        for (window_id, window) in extras {
            window.set_insets(crate::insets::reported(window_id));
        }
    }

    /// Mirrors an Activity's input focus onto its gpui window. Losing focus
    /// keeps the platform's active window: focus also moves to system UI,
    /// such as the notification shade, and Zed opens things in the active
    /// window.
    fn window_focus_changed(&self, window_ptr: &AndroidWindowStatePtr, focused: bool) {
        if focused {
            let handle = window_ptr.state.borrow().handle;
            self.common.borrow_mut().active_window = Some(handle);
        }
        window_ptr.set_active(focused);
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

        let mut touch_moves = PendingTouchMoves::default();
        while let Ok(event) = rx.try_recv() {
            if !matches!(event, ExtraWindowEvent::Motion { .. }) {
                touch_moves.flush();
            }
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
                ExtraWindowEvent::SurfaceDestroyed {
                    window_id,
                    detached,
                } => {
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    if let Some(state) = entry {
                        state.detach_surface();
                    }
                    drop(detached);
                }
                ExtraWindowEvent::SurfaceCreated {
                    window_id,
                    native_window,
                } => {
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    if let Some(state) = entry {
                        if let Err(error) =
                            state.attach_surface(native_window, self.compute_scale_factor())
                        {
                            log::error!(
                                "drain_extra_window_events: attach_surface for windowId={window_id} failed: {error:#}"
                            );
                        }
                    } else {
                        // Its window was never opened: the wait for its first
                        // surface timed out.
                        log::info!(
                            "drain_extra_window_events: SurfaceCreated for unknown windowId={window_id}; finishing its Activity"
                        );
                        crate::multi_window::finish_extra_activity(&self.android_app, window_id);
                    }
                }
                ExtraWindowEvent::OsClosed { window_id } => {
                    // OS-initiated close (user clicked chrome X). Set the
                    // os_closed flag so AndroidWindow::Drop will skip its
                    // JNI finishActivity call (Activity is already gone),
                    // then fire the gpui-registered `on_close` callback.
                    // gpui's callback drives `Window::remove_window()`, which
                    // in turn drops our `Box<dyn PlatformWindow>`, and
                    // `reap_closed_extra_windows` drops the map entry.
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    let Some(state) = entry else {
                        log::info!(
                            "drain_extra_window_events: OsClosed for already-removed windowId={window_id}"
                        );
                        crate::multi_window::unmark_window_registered(window_id);
                        continue;
                    };
                    state.state.borrow_mut().os_closed = true;
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
                ExtraWindowEvent::Motion { window_id, motion } => {
                    let entry = if window_id == crate::insets::PRIMARY_WINDOW_ID {
                        self.common.borrow().window.clone()
                    } else {
                        self.common.borrow().extra_windows.get(&window_id).cloned()
                    };
                    let Some(state) = entry else {
                        log::warn!(
                            "drain_extra_window_events: Motion for unknown windowId={window_id}"
                        );
                        continue;
                    };
                    let (translated, gliding) = {
                        let mut window_state = state.state.borrow_mut();
                        let scale = window_state.scale_factor;
                        let translated =
                            crate::events::translate_motion(motion, scale, &mut window_state);
                        (translated, window_state.trackpad_scroll.is_gliding())
                    };
                    for input in translated.inputs {
                        touch_moves.dispatch(&state, input);
                    }
                    if gliding {
                        request_frame_callback();
                    }
                }
                ExtraWindowEvent::Key {
                    window_id,
                    action,
                    device_id,
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
                        &state,
                        action,
                        device_id,
                        keycode,
                        meta_state,
                        repeat_count,
                    ) {
                        state.handle_input(input);
                    }
                }
                ExtraWindowEvent::FocusChanged { window_id, focused } => {
                    // A closing window's Activity loses focus after gpui
                    // has already dropped it, so an unknown id is expected.
                    let entry = self.common.borrow().extra_windows.get(&window_id).cloned();
                    if let Some(window_ptr) = entry {
                        self.window_focus_changed(&window_ptr, focused);
                    }
                }
            }
        }
        touch_moves.flush();

        self.common.borrow_mut().extra_event_rx = Some(rx);
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
    /// and hide the IME (per window) when it's lost. The
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
                // Gaining focus doesn't show the keyboard: as gpui's
                // `EntityInputHandler` docs ask, text fields request it
                // from a tap (and single-line editors when they gain
                // focus), as on Android. Seed the mirror so the IME has
                // current state when it shows.
                crate::ime::notify_text_state(window_ptr);
            } else {
                // IME going hidden — clear cached selection on this
                // window so the next show-transition re-seeds cleanly.
                window_ptr.state.borrow_mut().last_pushed_selection = None;
                crate::ime::request_keyboard(&app, extra_window_id, false);
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
        let (last_kind, last_selection, our_composition_active, composition_anchor) = {
            let state = window_ptr.state.borrow();
            (
                state.last_ime_target_kind,
                state.last_pushed_selection,
                state.ime_composition_text.is_some(),
                state.ime_composition_start.and_then(|start| {
                    state
                        .ime_composition_text
                        .as_ref()
                        .map(|text| (start, start + text.encode_utf16().count()))
                }),
            )
        };
        let Some((current_kind, current_selection, cleared_stale_mark)) = window_ptr
            .with_input_handler(|handler| {
                let kind = crate::ime::probe_target_kind(handler);
                let selection = handler
                    .selected_text_range(false)
                    .map(|selection| (selection.range.start, selection.range.end));
                // A marked range left from an earlier composition session
                // (typed in the editor, switched away without committing, came
                // back) would be replaced by the next setComposingText.
                let stale_mark = !our_composition_active && handler.marked_text_range().is_some();
                if stale_mark {
                    handler.unmark_text();
                }
                (Some(kind), selection, stale_mark)
            })
        else {
            return;
        };
        if cleared_stale_mark {
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

    /// Tells each Activity when a setting it mirrors changes. A window opened
    /// later gets the values in `open_extra_window`.
    fn tick_window_settings(&self) {
        let extras_row = crate::programming_extras_row_enabled();
        let soft_keyboard = crate::ime::on_screen_keyboard_enabled();
        let (extras_row_changed, soft_keyboard_changed) = {
            let mut common = self.common.borrow_mut();
            (
                common.last_extras_row_enabled.replace(extras_row) != Some(extras_row),
                common.last_soft_keyboard_setting.replace(soft_keyboard) != Some(soft_keyboard),
            )
        };
        // Only the main window has the extras row.
        if extras_row_changed
            && let Err(error) = crate::jni_env::call_window_method(
                &self.android_app,
                None,
                "setProgrammingExtrasRowEnabled",
                "(Z)V",
                &[extras_row.into()],
            )
        {
            log::warn!("setProgrammingExtrasRowEnabled: {error:#}");
        }
        if soft_keyboard_changed {
            crate::jni_env::call_all_windows(
                &self.android_app,
                "setSoftKeyboardEnabled",
                "(Z)V",
                &[soft_keyboard.into()],
            );
        }
    }

    /// Detect changes to `SOFT_KEYBOARD_VISIBLE` and force a redraw of the
    /// windows so the status bar keyboard button's `toggle_state` picks
    /// up the new value on the next paint. Runs every main-loop
    /// tick INDEPENDENTLY of input-handler visibility — the button
    /// shows whether or not a text field is focused, so gating this on `currently_visible` like
    /// `tick_ime_target_and_selection` would miss those cases.
    fn tick_soft_keyboard_visibility(&self) {
        let current = crate::ime::soft_keyboard_visible();
        let last = self.common.borrow().last_soft_keyboard_visible;
        if current == last {
            return;
        }
        self.common.borrow_mut().last_soft_keyboard_visible = current;
        self.redraw_keyboard_buttons();
    }

    /// When a physical keyboard starts or stops blocking the on-screen one,
    /// redraw the windows so the status bar keyboard button, hidden while
    /// it's blocked, follows, and hide an on-screen keyboard that's showing.
    fn tick_physical_keyboard(&self) {
        let blocks_ime = crate::ime::physical_keyboard_blocks_ime();
        if self.common.borrow().last_physical_keyboard_blocks_ime == blocks_ime {
            return;
        }
        self.common.borrow_mut().last_physical_keyboard_blocks_ime = blocks_ime;
        log::info!("ime: physical keyboard blocks the on-screen one={blocks_ime}");
        if blocks_ime && crate::ime::soft_keyboard_visible() {
            crate::jni_env::call_all_windows(
                &self.android_app,
                "requestIme",
                "(Z)V",
                &[false.into()],
            );
        }
        self.redraw_keyboard_buttons();
    }

    /// Forces a repaint of every window, each of which has a status bar
    /// keyboard button reading state gpui doesn't track.
    fn redraw_keyboard_buttons(&self) {
        let common = self.common.borrow();
        for window_ptr in common.window.iter().chain(common.extra_windows.values()) {
            window_ptr.state.borrow_mut().force_render = true;
        }
        FRAME_PENDING.store(true, std::sync::atomic::Ordering::Release);
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
        let mut touch_moves = PendingTouchMoves::default();
        loop {
            let read = iter.next(|event| match event {
                InputEvent::KeyEvent(key) => {
                    match crate::events::translate_key_event(&window_ptr, key) {
                        Some(input) => {
                            touch_moves.dispatch(&window_ptr, input);
                            InputStatus::Handled
                        }
                        None => InputStatus::Unhandled,
                    }
                }
                InputEvent::MotionEvent(motion) => {
                    let translated = {
                        let mut state = window_ptr.state.borrow_mut();
                        crate::events::translate_motion(
                            crate::events::Motion::from_event(motion),
                            scale_factor,
                            &mut state,
                        )
                    };
                    for input in translated.inputs {
                        touch_moves.dispatch(&window_ptr, input);
                    }
                    if translated.handled {
                        InputStatus::Handled
                    } else {
                        InputStatus::Unhandled
                    }
                }
                _ => InputStatus::Unhandled,
            });
            if !read {
                break;
            }
        }
        touch_moves.flush();
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
                let Some(native_window) = self.android_app.native_window() else {
                    return;
                };
                window_ptr.resize_surface(
                    native_window.width() as u32,
                    native_window.height() as u32,
                    self.compute_scale_factor(),
                );
            }
            MainEvent::ConfigChanged { .. } => self.resync_primary_window(),
            MainEvent::RedrawNeeded { .. } => {
                if let Some(window_ptr) = self.common.borrow().window.clone() {
                    window_ptr.refresh();
                }
            }
            MainEvent::GainedFocus | MainEvent::LostFocus => {
                let focused = matches!(event, MainEvent::GainedFocus);
                let primary = self.common.borrow().window.clone();
                if let Some(window_ptr) = primary {
                    self.window_focus_changed(&window_ptr, focused);
                }
            }
            MainEvent::Start => self.notify_app_lifecycle(AppLifecyclePhase::Foreground),
            MainEvent::Resume { .. } => self.notify_app_lifecycle(AppLifecyclePhase::Active),
            MainEvent::Pause => self.notify_app_lifecycle(AppLifecyclePhase::Inactive),
            MainEvent::Stop => self.notify_app_lifecycle(AppLifecyclePhase::Background),
            MainEvent::LowMemory => self.notify_memory_warning(),
            MainEvent::Destroy => {
                self.notify_app_lifecycle(AppLifecyclePhase::Disconnected);
                self.common.borrow_mut().running = false;
            }
            _ => {}
        }
    }

    fn notify_app_lifecycle(&self, phase: AppLifecyclePhase) {
        let callback = self.common.borrow_mut().callbacks.app_lifecycle.take();
        if let Some(mut callback) = callback {
            callback(phase);
            self.common.borrow_mut().callbacks.app_lifecycle = Some(callback);
        }
    }

    /// Files other apps opened with Zdroid (`open_files`). They stay queued
    /// until the app has registered `on_open_urls`.
    fn deliver_open_urls(&self) {
        if self.common.borrow().callbacks.open_urls.is_none() {
            return;
        }
        let Some(urls) = crate::open_files::take_pending_urls() else {
            return;
        };
        let callback = self.common.borrow_mut().callbacks.open_urls.take();
        if let Some(mut callback) = callback {
            callback(urls);
            self.common.borrow_mut().callbacks.open_urls = Some(callback);
        }
    }

    fn deliver_thermal_state_change(&self) {
        if !crate::thermal::take_changed() {
            return;
        }
        let callback = self
            .common
            .borrow_mut()
            .callbacks
            .thermal_state_change
            .take();
        if let Some(mut callback) = callback {
            callback();
            self.common.borrow_mut().callbacks.thermal_state_change = Some(callback);
        }
    }

    fn notify_memory_warning(&self) {
        let callback = self.common.borrow_mut().callbacks.memory_warning.take();
        if let Some(mut callback) = callback {
            callback();
            self.common.borrow_mut().callbacks.memory_warning = Some(callback);
        }
    }

    /// Density may have changed (rotation, dock/scaling). Refresh
    /// scale_factor and re-emit a resize so layout picks it up.
    fn resync_primary_window(&self) {
        let window_ptr = self.common.borrow().window.clone();
        let Some(window_ptr) = window_ptr else { return };
        let new_appearance = appearance_from_config(&self.android_app);
        self.common.borrow_mut().appearance = new_appearance;
        window_ptr.set_appearance(new_appearance);
        // Light/dark switches arrive only through the main Activity's
        // configuration, so extra windows follow it.
        let extra_windows: Vec<_> = self
            .common
            .borrow()
            .extra_windows
            .values()
            .cloned()
            .collect();
        for extra_window in extra_windows {
            extra_window.set_appearance(new_appearance);
        }
        let Some(native_window) = self.android_app.native_window() else {
            return;
        };
        window_ptr.resize_surface(
            native_window.width() as u32,
            native_window.height() as u32,
            self.compute_scale_factor(),
        );
    }

    /// `handle_main_event` while `open_extra_window` waits for the new
    /// Activity's surface. gpui is in the middle of an update then, so
    /// events that call back into gpui wait for the run loop. Surface
    /// changes can't wait: Android releases the old surface as soon as this
    /// returns.
    fn handle_main_event_while_opening_window(&self, event: android_activity::MainEvent<'_>) {
        use android_activity::MainEvent;
        match event {
            MainEvent::InitWindow { .. } | MainEvent::TerminateWindow { .. } => {
                self.handle_main_event(event)
            }
            MainEvent::Destroy => {
                self.defer_app_lifecycle(AppLifecyclePhase::Disconnected);
                self.common.borrow_mut().running = false;
            }
            MainEvent::WindowResized { .. }
            | MainEvent::ConfigChanged { .. }
            | MainEvent::RedrawNeeded { .. } => {
                self.common.borrow_mut().deferred_primary_resync = true;
            }
            MainEvent::GainedFocus | MainEvent::LostFocus => {
                self.common.borrow_mut().deferred_primary_focus =
                    Some(matches!(event, MainEvent::GainedFocus));
            }
            MainEvent::Start => self.defer_app_lifecycle(AppLifecyclePhase::Foreground),
            MainEvent::Resume { .. } => self.defer_app_lifecycle(AppLifecyclePhase::Active),
            MainEvent::Pause => self.defer_app_lifecycle(AppLifecyclePhase::Inactive),
            MainEvent::Stop => self.defer_app_lifecycle(AppLifecyclePhase::Background),
            MainEvent::LowMemory => self.common.borrow_mut().deferred_memory_warning = true,
            _ => {}
        }
    }

    fn defer_app_lifecycle(&self, phase: AppLifecyclePhase) {
        self.common.borrow_mut().deferred_lifecycle.push(phase);
    }

    fn remove_splash_after_first_frame(&self) {
        let mut common = self.common.borrow_mut();
        if std::mem::replace(&mut common.drew_first_frame, true) {
            return;
        }
        drop(common);
        log::info!("AndroidPlatform: first frame drawn");
        let result = crate::jni_env::call_window_method(
            &self.android_app,
            None,
            "onFirstFrameDrawn",
            "()V",
            &[],
        );
        if let Err(error) = result {
            log::warn!("onFirstFrameDrawn: {error:#}");
        }
    }

    /// Applies the `prevent_idle_sleep` guards. Here rather than in the guards
    /// themselves, which can be created or dropped on any thread, including
    /// inside android-activity's event callbacks.
    fn apply_keep_screen_on(&self) {
        let wanted = IDLE_SLEEP_GUARDS.load(Ordering::Acquire) > 0;
        if self.common.borrow().keep_screen_on == wanted {
            return;
        }
        self.common.borrow_mut().keep_screen_on = wanted;
        // `FLAG_KEEP_SCREEN_ON` only works while its own window is visible,
        // so every window gets it.
        crate::jni_env::call_all_windows(
            &self.android_app,
            "setKeepScreenOn",
            "(Z)V",
            &[wanted.into()],
        );
    }

    fn sync_window_visibility(&self) {
        let windows: Vec<crate::window::AndroidWindowStatePtr> = {
            let common = self.common.borrow();
            common
                .window
                .iter()
                .chain(common.extra_windows.values())
                .cloned()
                .collect()
        };
        for window_ptr in windows {
            window_ptr.sync_visibility();
        }
    }

    fn apply_deferred_main_events(&self) {
        let (resync, focus, lifecycle, memory_warning) = {
            let mut common = self.common.borrow_mut();
            (
                std::mem::take(&mut common.deferred_primary_resync),
                common.deferred_primary_focus.take(),
                std::mem::take(&mut common.deferred_lifecycle),
                std::mem::take(&mut common.deferred_memory_warning),
            )
        };
        for phase in lifecycle {
            self.notify_app_lifecycle(phase);
        }
        if memory_warning {
            self.notify_memory_warning();
        }
        if resync {
            self.resync_primary_window();
        }
        if let Some(focused) = focus {
            let primary = self.common.borrow().window.clone();
            if let Some(window_ptr) = primary {
                self.window_focus_changed(&window_ptr, focused);
            }
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

    fn gestures(&self) -> Option<Rc<dyn PlatformGestures>> {
        Some(self.android_gestures())
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.common.borrow().text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        // gpui reads the gesture tuning as each window opens.
        self.android_gestures();
        crate::runtime_service::start(&self.android_app);
        crate::thermal::start();
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
            // Before sleeping, so events deferred while a window opened
            // during the last iteration aren't left waiting.
            self.apply_deferred_main_events();
            // Sleeps until there is work: an input event, a vsync
            // (`frame_callback` runs as a looper task and sets
            // FRAME_PENDING), a runnable from another thread (the dispatcher
            // wakes the looper), a main-thread event from android-activity,
            // or a JNI event (`wake_run_loop`). With the Choreographer
            // driving us, this loop ticks at the panel's refresh rate while
            // frames run and not at all while idle.
            self.android_app.poll_events(None, |event| match event {
                android_activity::PollEvent::Wake => {}
                android_activity::PollEvent::Timeout => {}
                android_activity::PollEvent::Main(main_event) => {
                    log::trace!("MainEvent: {main_event:?}");
                    self.handle_main_event(main_event);
                }
                _ => {}
            });

            // Drain main-thread runnables enqueued from background threads. The
            // AndroidAppWaker wakes poll_events above when there's work; we drain
            // each tick regardless to catch anything between waker and poll.
            // Only the runnables already queued run in this pass. One that
            // re-enqueues itself from inside `run()` (a future whose own waker
            // fires during its poll) would otherwise keep the iterator going
            // and starve the input and paint steps below: the window freezes
            // at full CPU instead of merely running hot.
            let receiver = self.common.borrow().main_receiver.clone();
            let batch: Vec<_> = receiver.try_iter().collect();
            for runnable in batch.into_iter().flatten() {
                runnable.run();
            }

            // Drain input events into the active window. We call this every
            // tick rather than only on Wake — android-activity gates input
            // delivery on this iterator being polled, so missing a tick can
            // stall touch.
            self.drain_input_events();

            // Drain JNI-side extra-window lifecycle / touch events.
            self.drain_extra_window_events();
            self.reap_closed_extra_windows();

            self.deliver_open_urls();
            self.deliver_thermal_state_change();

            self.apply_window_insets();

            // Drain JNI-side IME events (commit/composition/key/delete
            // delivered by `ZdroidInputConnection` on Android's UI
            // thread). Dispatches into the primary window's
            // `PlatformInputHandler` / `handle_input`.
            self.drain_ime_events();

            self.tick_soft_keyboard_visibility();
            self.tick_physical_keyboard();
            self.tick_window_settings();

            self.sync_window_visibility();
            self.apply_keep_screen_on();

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
                // A trackpad scroll's glide steps on the vsync, before the
                // windows paint it.
                for window_ptr in primary.iter().chain(&extras) {
                    advance_trackpad_glide(window_ptr);
                }
                // While frames that present run back to back (scrolling,
                // animations), ask for the next vsync before painting, as
                // Android's own animations do: a paint that overruns the
                // frame budget then starts the next frame right away
                // instead of waiting for a further vsync. A run ends with
                // up to two frames that present nothing.
                if LAST_PRESENT_AT
                    .take()
                    .is_some_and(|at| at.elapsed() < FRAME_RUN_GAP)
                {
                    request_frame_callback();
                }
                let has_main_window = primary.is_some();
                // Hidden windows have no surface to present to; gpui's
                // contract is that the platform doesn't request their frames.
                for window_ptr in primary.into_iter().chain(extras) {
                    if window_ptr.is_visible() {
                        window_ptr.refresh();
                    }
                }
                if has_main_window {
                    self.remove_splash_after_first_frame();
                }

                // Reconcile IME visibility against each window's
                // `input_handler` presence. Edge-triggered at frame
                // boundaries — gpui's take/set oscillation within each
                // paint can't leak through because we're sampling after
                // it, not inside set/take callbacks. Only after a frame:
                // gpui sets input handlers while painting, and anything
                // that moves the selection or composition repaints, so an
                // idle tick has nothing new to push, and each check calls
                // into the focused editor three times.
                self.reconcile_ime_visibility();
            }
        }

        log::info!("AndroidPlatform::run: exiting event loop");
        // A Destroy that arrived while a window was opening.
        self.apply_deferred_main_events();
        let quit = self.common.borrow_mut().callbacks.quit.take();
        if let Some(mut fun) = quit {
            if !fun() {
                log::warn!("AndroidPlatform::run: app was busy, shutdown skipped");
            }
        }
        if self.common.borrow().exit_requested {
            // gpui's quit hook only stops the run loop. On Android the
            // hosting GameActivity stays alive (its JVM-side thread keeps
            // running independently of `android_main` returning), so the
            // user-visible result of a "Quit" menu click would be: editor
            // stops responding to gestures but the window doesn't dismiss.
            // Exit the process so `MainActivity.onDestroy` runs (it does
            // its own `Process.killProcess` fallback + cursor / splash
            // teardown) and the OS returns the user to the launcher.
            crate::runtime_service::shutdown(&self.android_app);
            std::process::exit(0);
        }
    }

    fn quit(&self) {
        // App::quit runs inside an app update, where the quit callback can't
        // borrow the app and skips App::shutdown, so quit observers (workspace
        // serialization and the like) would never run. Stop the loop instead;
        // run() invokes the callback once the update has unwound, as the
        // Linux and Windows backends do.
        let mut common = self.common.borrow_mut();
        common.running = false;
        common.exit_requested = true;
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {}
    fn activate(&self, _ignoring_other_apps: bool) {}
    fn hide(&self) {}
    fn hide_other_apps(&self) {}
    fn unhide_other_apps(&self) {}

    fn on_displays_changed(&self, _callback: Box<dyn FnMut()>) {}

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
            self.android_app
                .poll_events(Some(std::time::Duration::from_millis(100)), |event| {
                    if let android_activity::PollEvent::Main(main_event) = event {
                        log::trace!("MainEvent during open_window block: {main_event:?}");
                        self.handle_main_event_while_opening_window(main_event);
                    }
                });

            // NOTE: do not drain main_receiver here. open_window runs inside
            // gpui's `cx.update` borrow guard; a runnable that calls
            // `cx.update(...)` re-enters the borrow and panics ("RefCell
            // already borrowed"). The outer event loop drains the queue on
            // the next tick, so queued work picks up fine.
        }

        let appearance = self.common.borrow().appearance;
        let gpu_context = self.common.borrow().gpu_context.clone();
        let window = AndroidWindow::new(
            handle,
            options,
            gpu_context,
            appearance,
            self.android_app.clone(),
            self.android_gestures().tuning,
        );

        let native_window = self.android_app.native_window().ok_or_else(|| {
            anyhow::anyhow!("open_window: native_window vanished between poll and attach")
        })?;
        let scale_factor = self.compute_scale_factor();
        window.ptr().attach_surface(native_window, scale_factor)?;
        // `MainActivity` reports its insets long before gpui opens its window.
        window
            .ptr()
            .set_insets(crate::insets::reported(crate::insets::PRIMARY_WINDOW_ID));

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

    /// Android's picker is for other apps' documents and hands back content
    /// URIs Zed can't open by path, so the app shows Zed's own picker. That
    /// picker takes one path of either kind, so `options` doesn't apply.
    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let paths = request_path_prompt(false, None);
        let (result, receiver) = oneshot::channel();
        self.foreground_executor()
            .spawn(async move {
                // The picker's "open this directory" answers `folder/./`, which
                // callers would keep in the paths they save and show.
                let paths = paths.await.ok().flatten().map(|paths| {
                    paths
                        .iter()
                        .map(|path| path.components().collect::<PathBuf>())
                        .collect()
                });
                result.send(Ok(paths)).ok();
            })
            .detach();
        receiver
    }

    /// Zed's picker starts in the project's folder, as it does for Save As, so
    /// `directory` doesn't apply.
    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let paths = request_path_prompt(true, suggested_name.map(str::to_owned));
        let (result, receiver) = oneshot::channel();
        self.foreground_executor()
            .spawn(async move {
                let path = paths
                    .await
                    .ok()
                    .flatten()
                    .and_then(|paths| paths.into_iter().next());
                result.send(Ok(path)).ok();
            })
            .detach();
        receiver
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }
    fn reveal_path(&self, path: &Path) {
        call_activity_with_path(&self.android_app, "revealInFiles", path);
    }
    fn open_with_system(&self, path: &Path) {
        call_activity_with_path(&self.android_app, "openWithSystem", path);
    }

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.common.borrow_mut().callbacks.quit = Some(callback);
    }
    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        self.common.borrow_mut().callbacks.app_lifecycle = Some(callback);
    }
    fn on_memory_warning(&self, callback: Box<dyn FnMut()>) {
        self.common.borrow_mut().callbacks.memory_warning = Some(callback);
    }
    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}
    fn on_system_sleep(&self, _callback: Box<dyn FnMut()>) {
        // Same as on_system_wake: no app-visible system sleep signal.
    }
    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {
        // Android exposes no system sleep/wake signal to apps; process
        // lifecycle (onResume etc.) is a different concept and already
        // flows through the app-lifecycle path. Drop the callback.
    }
    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}
    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}
    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}
    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}
    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn thermal_state(&self) -> ThermalState {
        crate::thermal::state()
    }
    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.common.borrow_mut().callbacks.thermal_state_change = Some(callback);
    }
    /// Keeps the screen on while the app is in front, Android's equivalent:
    /// Zed asks for this while an agent works. Counted, so overlapping
    /// requests don't release it early; the run loop applies the result.
    fn prevent_idle_sleep(&self, reason: &str) -> Task<Result<ActivityGuard>> {
        log::debug!("keeping the screen on: {reason}");
        IDLE_SLEEP_GUARDS.fetch_add(1, Ordering::AcqRel);
        wake_run_loop();
        Task::ready(Ok(ActivityGuard::new(|| {
            IDLE_SLEEP_GUARDS.fetch_sub(1, Ordering::AcqRel);
            wake_run_loop();
        })))
    }

    fn compositor_name(&self) -> &'static str {
        "Android"
    }
    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow::anyhow!(
            "app_path is not yet implemented on Android"
        ))
    }
    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow::anyhow!(
            "auxiliary executables are not available on Android"
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        crate::cursor::set_pointer_icon(&self.android_app, style);
    }
    fn hide_cursor_until_mouse_moves(&self) {
        crate::cursor::hide_until_pointer_moves(&self.android_app);
    }
    fn is_cursor_visible(&self) -> bool {
        crate::cursor::is_visible()
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

    // Keystore operations are binder calls (and key generation can take
    // hundreds of milliseconds), so keep them off the thread that renders.
    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        let android_app = self.android_app.clone();
        let (url, username, password) = (url.to_owned(), username.to_owned(), password.to_vec());
        self.background_executor().spawn(async move {
            crate::credentials::write(&android_app, &url, &username, &password)
        })
    }
    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        let android_app = self.android_app.clone();
        let url = url.to_owned();
        self.background_executor()
            .spawn(async move { crate::credentials::read(&android_app, &url) })
    }
    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        let android_app = self.android_app.clone();
        let url = url.to_owned();
        self.background_executor()
            .spawn(async move { crate::credentials::delete(&android_app, &url) })
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(AndroidKeyboardLayout)
    }
    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }
    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}
