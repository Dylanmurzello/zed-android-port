use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use android_activity::AndroidApp;
use anyhow::Result;
use futures::channel::oneshot;
use ndk::native_window::NativeWindow;
use raw_window_handle as rwh;

use gpui::{
    AnyWindowHandle, Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, Modifiers,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Pixels,
    FrameRequestSource, Point, PromptButton, PromptLevel, RequestFrameOptions, Scene, Size,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams,
    WindowVisibility, point, px, size,
};
use gpui_wgpu::{GpuContext, WgpuRenderer, WgpuSurfaceConfig, wgpu};

use crate::display::AndroidDisplay;
use crate::platform::set_native_window_frame_rate;

/// Raw window handle wrapper for Android. Holds a `*mut ANativeWindow` pointer
/// (obtained from `android_activity::AndroidApp::native_window()`), or null when
/// the surface is not currently attached (between `TerminateWindow` and the next
/// `InitWindow`).
///
/// `Send + Sync` are required by `WgpuRenderer::new`'s bounds; the pointer is
/// only ever dereferenced on the main thread, and wgpu uses it synchronously
/// during surface creation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AndroidRawWindow {
    pub(crate) native_window: *mut c_void,
}

unsafe impl Send for AndroidRawWindow {}
unsafe impl Sync for AndroidRawWindow {}

impl rwh::HasWindowHandle for AndroidRawWindow {
    fn window_handle(&self) -> std::result::Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let Some(non_null) = NonNull::new(self.native_window) else {
            return Err(rwh::HandleError::Unavailable);
        };
        let handle = rwh::AndroidNdkWindowHandle::new(non_null);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl rwh::HasDisplayHandle for AndroidRawWindow {
    fn display_handle(&self) -> std::result::Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let handle = rwh::AndroidDisplayHandle::new();
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(handle.into()) })
    }
}

#[derive(Default)]
pub(crate) struct Callbacks {
    pub(crate) request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    pub(crate) input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    pub(crate) active_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) hovered_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    pub(crate) moved: Option<Box<dyn FnMut()>>,
    pub(crate) should_close: Option<Box<dyn FnMut() -> bool>>,
    pub(crate) close: Option<Box<dyn FnOnce()>>,
    pub(crate) appearance_changed: Option<Box<dyn FnMut()>>,
}

pub(crate) struct AndroidWindowState {
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) scale_factor: f32,
    pub(crate) renderer: Option<WgpuRenderer>,
    pub(crate) raw_window: AndroidRawWindow,
    pub(crate) display: Rc<dyn PlatformDisplay>,
    pub(crate) input_handler: Option<PlatformInputHandler>,
    pub(crate) appearance: WindowAppearance,
    pub(crate) background_appearance: WindowBackgroundAppearance,
    pub(crate) handle: AnyWindowHandle,
    pub(crate) gpu_context: GpuContext,
    /// Holds an `ANativeWindow_acquire` refcount on the underlying native
    /// window so the pointer stored in `raw_window` stays valid for the
    /// lifetime of any Vulkan `VkSurfaceKHR` referencing it. Dropped (and
    /// thus refcount-decremented) on `detach_surface` and replaced when
    /// `attach_surface` runs with a fresh window.
    pub(crate) native_window: Option<NativeWindow>,
    /// After a successful `recover()` the atlas textures have been cleared,
    /// but gpui's invalidator doesn't know that. The next paint must be
    /// forced; consumed by `refresh()` via `std::mem::take`.
    pub(crate) force_render_after_recovery: bool,
    /// Set true by the platform's `OsClosed` drain handler when the
    /// underlying `ExtraWindowActivity` has already destroyed (user clicked
    /// the OS chrome X). `AndroidWindow::Drop` reads this to skip its
    /// JNI `finishAndRemoveTask` call — the Activity is already gone, so
    /// issuing the call would warn-log harmlessly. Lives on the state (in
    /// the `Rc<RefCell>`) rather than on `AndroidWindow` itself so it
    /// survives the `Box<dyn PlatformWindow>` drop and is reachable from the
    /// drain handler via the platform's `extra_windows` map.
    pub(crate) os_closed: AtomicBool,
    /// Cheap clone (Arc internally) used by `AndroidWindow::Drop` to issue
    /// `multi_window::finish_extra_activity` for extra windows. Always set
    /// at construction time (both primary and extras carry it; primary just
    /// never reaches the Drop branch that uses it).
    pub(crate) android_app: AndroidApp,
    /// Per-window click + button-state tracker (shared by mouse and
    /// touch dispatchers). See `events/click_track.rs`. Lives here as
    /// a field so each window has its own click run + non-primary
    /// held-button latch — a click in window A doesn't bleed into
    /// window B's double-click run.
    pub(crate) clicks: crate::events::click_track::ClickTrackState,
    /// Per-window captured-pointer (trackpad) SM state. See
    /// `captured_pointer.rs`. Hold-drag timers, drag-lock state,
    /// tap-tap-drag pending flags, etc. all live here so a gesture in
    /// one window doesn't leak into another's.
    pub(crate) captured: crate::captured_pointer::CapturedPointerState,
    /// Per-window native-touch SM state. See `crate::touch`. Per-pointer-
    /// ID tracking, multi-finger gesture classification, tap vs scroll
    /// discrimination. Each window's touchscreen events are isolated.
    pub(crate) touch: crate::touch::TouchState,
    /// Trackpad-mode counterpart to `touch`. Receives the same
    /// `TouchEvent` stream when `android_input.trackpad_mode` is on
    /// (the dispatcher branches via the
    /// `ime::TRACKPAD_MODE_ENABLED` atomic).
    pub(crate) trackpad_touch: crate::touch::TrackpadTouchState,
    /// Set by elements (scrollbar thumb, splitter, resize handle) while
    /// they're actively driving a direct-manipulation drag. Flips via
    /// `PlatformWindow::set_drag_active` on this window's
    /// `AndroidWindow`. The touch SM checks this in its `Move` handler:
    /// while set, single-finger drag emits `MouseMove(Left held)` so
    /// the dragging element receives motion instead of having the SM
    /// auto-convert to `ScrollWheel`. Without this gate, a touch-drag
    /// on a scrollbar thumb gets clobbered by the threshold-cross
    /// `MouseUp(0)` cancel.
    pub(crate) drag_active: AtomicBool,
    /// True when the most recent pointer event on this window was
    /// touch (Finger). Set in `events.rs` and `touch.rs` for Finger
    /// dispatch; cleared for mouse / stylus / captured-trackpad. UI
    /// components query via `Window::last_input_was_touch` and use it
    /// to widen hit zones for touch without affecting mouse
    /// precision (e.g. the pane splitter MouseDown handler).
    pub(crate) last_input_was_touch: AtomicBool,
    /// Mirror of [`AndroidWindow::extra_window_id`] — set by
    /// `cx.open_window` after the wrapping `AndroidWindow` is
    /// built. None for the primary surface, `Some(id)` for
    /// `ExtraWindowActivity`-backed windows. Read by the touch
    /// trackpad SM so cursor-sprite JNI calls route to the
    /// correct Activity (`MainActivity` vs the right extra).
    pub(crate) extra_window_id: Option<u64>,
    /// IME composition state we maintain ourselves rather than asking
    /// `handler.marked_text_range()` for it. The handler's marked-text
    /// support varies by input view (the editor implements it
    /// correctly, the terminal effectively doesn't), so trusting it
    /// produced duplicate-letter and lost-on-enter bugs. We track the
    /// composition's start (UTF-16 index when first setComposingText
    /// fired) and the last text we set; on each new setComposingText
    /// we replace `start..start + last_text.utf16_len()` with the new
    /// text. Cleared on commit / finish / cancel. Phase 2 will add
    /// proper marked-text composition for the editor; this Phase 1
    /// behavior is "live commit" — composition immediately visible
    /// (no underline preview), which is correct for terminal and
    /// adequate for editor ASCII typing.
    pub(crate) ime_composition_start: Option<usize>,
    pub(crate) ime_composition_text: Option<String>,
    /// Per-window mirror of whether the input handler was present at
    /// the last frame-boundary IME reconcile. Compared against the
    /// current `input_handler.is_some()` to detect show/hide
    /// transitions. Per-window so that an `ExtraWindowActivity`
    /// (settings, picker) can drive its own IME independent of the
    /// primary surface — without this, focusing a text input in a
    /// spawned window wouldn't trigger show_keyboard because the
    /// global was already true for MainActivity.
    pub(crate) ime_currently_visible: bool,
    /// Per-window cache of the last classified IME target kind
    /// (terminal vs editor). On change we issue restartInput on this
    /// window's Activity so the IME's EditorInfo reflects the new
    /// pane type (terminal: VISIBLE_PASSWORD/no autocorrect; editor:
    /// VISIBLE_PASSWORD/no suggestions). See `ime::probe_target_kind`.
    pub(crate) last_ime_target_kind: Option<crate::ime::ImeTargetKind>,
    /// Per-window cache of the last (start, end) selection we pushed
    /// to this window's Kotlin Activity via `updateImeTextState`.
    /// Compared each tick against the editor's current selection; on
    /// diff we re-push so the IME observes touch-driven cursor moves
    /// in this specific window.
    pub(crate) last_pushed_selection: Option<(usize, usize)>,
}

#[derive(Clone)]
pub(crate) struct AndroidWindowStatePtr {
    pub(crate) state: Rc<RefCell<AndroidWindowState>>,
    pub(crate) callbacks: Rc<RefCell<Callbacks>>,
}

impl AndroidWindowStatePtr {
    /// Called from the platform run loop on `MainEvent::InitWindow` (and from
    /// `open_window` on first attach). Wires the new `ANativeWindow` surface
    /// into the renderer, creating it on first call and replacing the surface
    /// on subsequent calls (e.g. after a rotation that destroys and recreates
    /// the window).
    ///
    /// On first call, this is also where the shared `WgpuContext` (device,
    /// queue, adapter) gets created — the gpu_context cell starts empty and
    /// `WgpuRenderer::new` populates it.
    ///
    /// Takes ownership of `NativeWindow` so the underlying refcount on
    /// `ANativeWindow*` is held for the life of this surface; the wrapper is
    /// dropped on `detach_surface` or replaced on subsequent `attach_surface`.
    ///
    /// `scale_factor` is the device's display density multiplier (160 dpi = 1.0,
    /// 320 dpi = 2.0, etc.). Set on first attach so layout uses the correct DPI
    /// from frame zero.
    pub(crate) fn attach_surface(
        &self,
        native_window: NativeWindow,
        scale_factor: f32,
    ) -> Result<()> {
        let width = native_window.width() as u32;
        let height = native_window.height() as u32;
        let raw_window = AndroidRawWindow {
            native_window: native_window.ptr().as_ptr().cast(),
        };
        let config = WgpuSurfaceConfig {
            size: size(
                DevicePixels(width.max(1) as i32),
                DevicePixels(height.max(1) as i32),
            ),
            // Alpha-aware compositor mode (PreMultiplied with
            // Inherit fallback). The SurfaceView's pixel format is
            // what actually decides the compositor's alpha-awareness
            // on Adreno+Samsung: MainActivity (GameActivity) ends up
            // OPAQUE and ExtraWindowActivity's holder is explicitly
            // OPAQUE too, so the wgpu surface's alpha_mode falls back
            // to Inherit and behaves opaque regardless of this flag.
            // Leaving `true` keeps wgpu happy on the rare path where
            // a future window does want alpha blending (e.g. the
            // splash overlay's pre-first-paint window-background
            // bridge), without affecting the post-first-paint editor
            // rendering. The visible-tint regression had nothing to
            // do with this flag — see ExtraWindowActivity.kt for the
            // real fix.
            transparent: true,
            // Mailbox lets the swap chain discard a stale frame at present
            // time when a newer one is ready, which under irregular paint
            // cadence (scroll bursts, typing flurries) feels ~1 frame
            // tighter than FIFO. Falls back to Fifo inside the renderer
            // if the surface doesn't expose Mailbox; Adreno 740 (Tab S9)
            // does.
            //
            // Triple-buffer (`Some(3)`) was tried and reverted: at 120Hz
            // the cost is +8.33ms of in-flight latency which is invisible,
            // but Samsung's smart-refresh aggressively drops the panel to
            // 60/30Hz when the app isn't actively rendering, and at those
            // rates the extra image adds 16-33ms of perceived input
            // latency that the user feels as overall sluggishness. Stay
            // at the wgpu default of 2 until we have a way to keep the
            // panel pinned at 120Hz.
            preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
        };

        let mut state = self.state.borrow_mut();
        state.raw_window = raw_window;
        state.scale_factor = scale_factor;
        state.bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(
                px(width as f32 / scale_factor),
                px(height as f32 / scale_factor),
            ),
        };

        if state.renderer.is_some() {
            let gpu_context = state.gpu_context.clone();
            let ctx_ref = gpu_context.borrow();
            let instance = ctx_ref
                .as_ref()
                .map(|ctx| ctx.instance.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("attach_surface: gpu_context missing on re-attach")
                })?;
            drop(ctx_ref);
            state
                .renderer
                .as_mut()
                .unwrap()
                .replace_surface(&raw_window, config, &instance)?;
            log::info!("AndroidWindow::attach_surface: replaced surface ({width}x{height})");
        } else {
            let gpu_context = state.gpu_context.clone();
            let mut renderer = WgpuRenderer::new(gpu_context, &raw_window, config, None)?;
            // Brand-color clear so the very first wgpu frame replaces
            // the SurfaceView's default-black buffer with brand indigo
            // instead of a black flash between SurfaceView attach and
            // the first scene paint. Matches `@color/zdroid_bg` (#1E1E2E
            // = 30/255 ≈ 0.1176) so the visual handoff is:
            //   SplashActivity AVD → MainActivity windowBackground
            //   (static icon over indigo) → SurfaceView indigo → editor
            // with no black gap anywhere. Desktop wgpu embedders keep
            // the default transparent clear; this is an Android-only
            // override because we own the entire surface.
            renderer.set_clear_color(wgpu::Color {
                r: 30.0 / 255.0,
                g: 30.0 / 255.0,
                b: 46.0 / 255.0,
                a: 1.0,
            });
            state.renderer = Some(renderer);
            log::info!("AndroidWindow::attach_surface: created renderer ({width}x{height})");
        }

        // Ask the system for the panel's maximum refresh rate. On API 30+
        // capable devices this opts us into 120Hz on Tab S9 Ultra / Pixel
        // Tablet (otherwise the compositor leaves us at 60Hz for "compat"
        // apps regardless of panel capability). No-op on API 26-29 — the
        // symbol is resolved via `dlsym` rather than direct-linked so
        // missing-symbol on older devices is silent, not a load-time
        // crash. Issue every attach so re-attach after rotation /
        // background-resume re-asserts the hint.
        set_native_window_frame_rate(raw_window.native_window);

        // Replace the previous wrapper (if any). The drop releases the prior
        // ANativeWindow refcount; Vulkan's VkSurfaceKHR holds its own ref.
        state.native_window = Some(native_window);

        // Force a paint on the next refresh tick. After a fresh `replace_surface`
        // the swapchain is uninitialized (presents black), and gpui's
        // request_frame is a no-op when the invalidator hasn't been dirtied.
        // On first attach this is a no-op duplicate of gpui's own initial
        // `window.draw(cx)` inside open_window, but it's cheap and makes the
        // background→foreground path symmetric.
        state.force_render_after_recovery = true;
        Ok(())
    }

    /// Called from the platform run loop on `MainEvent::TerminateWindow`. The
    /// `ANativeWindow` is being destroyed by the system. We unconfigure the
    /// wgpu surface so subsequent draws bail out, but keep the device, queue,
    /// and atlas alive so the next `InitWindow` can `replace_surface` cheaply
    /// without rebuilding glyph caches.
    pub(crate) fn detach_surface(&self) {
        // Keep the renderer alive — gpui's element tree caches
        // `AtlasTextureId`s into our atlas. Dropping the renderer (and
        // therefore the atlas) leaves those ids dangling, and the next
        // paint indexes into an empty `WgpuAtlasStorage`, panicking at
        // `wgpu_atlas.rs:79`. `unconfigure_surface` clears the swapchain
        // but keeps the VkSurfaceKHR + atlas + pipelines.
        let mut state = self.state.borrow_mut();
        if let Some(renderer) = state.renderer.as_mut() {
            renderer.unconfigure_surface();
        }
        state.raw_window = AndroidRawWindow {
            native_window: std::ptr::null_mut(),
        };
        state.native_window = None;
        log::info!("AndroidWindow::detach_surface: surface unconfigured");
    }

    /// Called on `MainEvent::WindowResized` and `MainEvent::ConfigChanged`.
    /// Both can change the visible size or DPI (rotation, dock/scaling), so
    /// the platform layer is expected to recompute scale_factor each call
    /// rather than reuse the stored one.
    ///
    /// Fires the `on_resize` callback that gpui registered so it relays out
    /// the element tree at the new size + DPI.
    pub(crate) fn resize_surface(&self, width: u32, height: u32, scale_factor: f32) {
        let content_size = {
            let mut state = self.state.borrow_mut();
            state.scale_factor = scale_factor;
            state.bounds = Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(
                    px(width as f32 / scale_factor),
                    px(height as f32 / scale_factor),
                ),
            };
            if let Some(renderer) = state.renderer.as_mut() {
                renderer.update_drawable_size(size(
                    DevicePixels(width.max(1) as i32),
                    DevicePixels(height.max(1) as i32),
                ));
            }
            state.bounds.size
        };

        if let Some(callback) = self.callbacks.borrow_mut().resize.as_mut() {
            callback(content_size, scale_factor);
        }
    }

    /// Drive a paint cycle: invoke the `request_frame` callback that gpui
    /// registered. The callback walks the element tree, builds a `Scene`, and
    /// calls back into our `PlatformWindow::draw` to actually submit it.
    ///
    /// The take/restore dance mirrors X11's `refresh` — re-entrant `refresh`
    /// calls during the callback's own paint side-effects would otherwise
    /// double-borrow the callback Box.
    pub(crate) fn refresh(&self) {
        let force_render = std::mem::take(&mut self.state.borrow_mut().force_render_after_recovery);
        let callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation: false,
                force_render,
                signal_at: None,
                signal_source: FrameRequestSource::NativeCallback,
            });
            self.callbacks.borrow_mut().request_frame = Some(callback);
        }
    }

    pub(crate) fn set_appearance(&self, appearance: WindowAppearance) {
        if self.state.borrow().appearance == appearance {
            return;
        }
        self.state.borrow_mut().appearance = appearance;
        let callback = self.callbacks.borrow_mut().appearance_changed.take();
        if let Some(mut callback) = callback {
            callback();
            self.callbacks.borrow_mut().appearance_changed = Some(callback);
        }
    }

    /// Fire the gpui-registered `on_active_status_change` callback. gpui
    /// uses this to drive `cx.observe_window_activation` listeners — the
    /// editor crate registers one of these in `Editor::new` (editor.rs:2618)
    /// to enable the cursor blink animation when the window becomes active.
    /// Our `is_active()` returns true at construction, but gpui only fires
    /// the activation observers when this callback runs — so without
    /// invoking it explicitly, the editor never calls
    /// `BlinkManager::enable`, and the cursor renders statically until the
    /// user's first input flips the path through `pause_blinking`.
    pub(crate) fn notify_active_status_change(&self, active: bool) {
        let callback = self.callbacks.borrow_mut().active_status_change.take();
        if let Some(mut callback) = callback {
            callback(active);
            self.callbacks.borrow_mut().active_status_change = Some(callback);
        }
    }

    /// Dispatches a translated input event into gpui via the registered
    /// `on_input` callback, then routes printable `KeyDown`s through the
    /// active `PlatformInputHandler` (gpui's text-input path) when the
    /// callback didn't claim them.
    pub(crate) fn handle_input(&self, input: PlatformInput) {
        let callback = self.callbacks.borrow_mut().input.take();
        if let Some(mut callback) = callback {
            let result = callback(input.clone());
            self.callbacks.borrow_mut().input = Some(callback);
            if !result.propagate {
                return;
            }
        }
        if let PlatformInput::KeyDown(event) = input {
            // Only allow shift as the modifier when inserting text — anything
            // else (ctrl-c, alt-anything) is presumed to be a binding.
            if event.keystroke.modifiers.is_subset_of(&Modifiers::shift()) {
                let mut state = self.state.borrow_mut();
                if let Some(mut input_handler) = state.input_handler.take() {
                    if let Some(key_char) = &event.keystroke.key_char {
                        drop(state);
                        input_handler.replace_text_in_range(None, key_char);
                        state = self.state.borrow_mut();
                    }
                    state.input_handler = Some(input_handler);
                }
            }
        }
    }
}

pub(crate) struct AndroidWindow {
    pub(crate) ptr: AndroidWindowStatePtr,
    /// `Some(window_id)` when this is an extra (multi-window) host backed
    /// by an `ExtraWindowActivity`. `None` for the GameActivity-owned
    /// primary window. On `Drop`, an extra window calls
    /// `multi_window::finish_extra_activity` to ask the JVM to finish the
    /// Activity (removing it from screen and Recents) — unless the
    /// `os_closed` flag on state is already set, in which case the Activity
    /// destroyed itself first (user clicked OS chrome X) and the JNI call
    /// would warn-log harmlessly.
    pub(crate) extra_window_id: Option<u64>,
}

impl AndroidWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        _params: WindowParams,
        gpu_context: GpuContext,
        appearance: WindowAppearance,
        android_app: AndroidApp,
    ) -> Self {
        let display: Rc<dyn PlatformDisplay> = Rc::new(AndroidDisplay::new());
        let bounds = display.bounds();

        let state = AndroidWindowState {
            bounds,
            scale_factor: 1.0,
            renderer: None,
            raw_window: AndroidRawWindow {
                native_window: std::ptr::null_mut(),
            },
            display,
            input_handler: None,
            appearance,
            background_appearance: WindowBackgroundAppearance::Opaque,
            handle,
            gpu_context,
            native_window: None,
            force_render_after_recovery: false,
            os_closed: AtomicBool::new(false),
            android_app,
            clicks: crate::events::click_track::ClickTrackState::default(),
            captured: crate::captured_pointer::CapturedPointerState::default(),
            touch: crate::touch::TouchState::default(),
            trackpad_touch: crate::touch::TrackpadTouchState::default(),
            drag_active: AtomicBool::new(false),
            last_input_was_touch: AtomicBool::new(false),
            extra_window_id: None,
            ime_composition_start: None,
            ime_composition_text: None,
            ime_currently_visible: false,
            last_ime_target_kind: None,
            last_pushed_selection: None,
        };

        Self {
            ptr: AndroidWindowStatePtr {
                state: Rc::new(RefCell::new(state)),
                callbacks: Rc::new(RefCell::new(Callbacks::default())),
            },
            extra_window_id: None,
        }
    }

    pub(crate) fn ptr(&self) -> AndroidWindowStatePtr {
        self.ptr.clone()
    }
}

impl Drop for AndroidWindow {
    fn drop(&mut self) {
        let Some(window_id) = self.extra_window_id else {
            return;
        };
        let state = self.ptr.state.borrow();
        if state.os_closed.load(std::sync::atomic::Ordering::SeqCst) {
            // OS-initiated close already happened — Activity is gone, no
            // need to issue another finish call.
            return;
        }
        let android_app = state.android_app.clone();
        drop(state);
        crate::multi_window::finish_extra_activity(&android_app, window_id);
    }
}

impl rwh::HasWindowHandle for AndroidWindow {
    fn window_handle(&self) -> std::result::Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let raw = self.ptr.state.borrow().raw_window;
        let Some(non_null) = NonNull::new(raw.native_window) else {
            return Err(rwh::HandleError::Unavailable);
        };
        let handle = rwh::AndroidNdkWindowHandle::new(non_null);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl rwh::HasDisplayHandle for AndroidWindow {
    fn display_handle(&self) -> std::result::Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let handle = rwh::AndroidDisplayHandle::new();
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(handle.into()) })
    }
}

impl PlatformWindow for AndroidWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.ptr.state.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Maximized(self.ptr.state.borrow().bounds)
    }

    fn content_size(&self) -> Size<Pixels> {
        self.ptr.state.borrow().bounds.size
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        self.ptr.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.ptr.state.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.ptr.state.borrow().display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        // No show_keyboard call here — gpui's paint cycle calls
        // take_input_handler/set_input_handler every frame for state
        // shuffling (window.rs line 2497-2509), which would oscillate
        // the IME visibly. Show/hide is driven by a frame-boundary
        // diff in `AndroidPlatform::reconcile_ime_visibility` instead.
        self.ptr.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        // Same reasoning as `set_input_handler`: no hide_keyboard here.
        self.ptr.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        // Only meaningful for extra windows — primary GameActivity is
        // always at the foreground of its task. settings_ui's
        // existing-window dedup (settings_ui.rs:622) calls this after
        // finding an open SettingsWindow; without it, tapping
        // "Open Settings" again while Settings is already open silently
        // no-ops (the existing window stays in the background).
        let Some(window_id) = self.extra_window_id else {
            return;
        };
        let android_app = self.ptr.state.borrow().android_app.clone();
        crate::multi_window::activate_extra_activity(&android_app, window_id);
    }

    fn is_active(&self) -> bool {
        true
    }

    fn visibility(&self) -> WindowVisibility {
        // Backgrounding tears down the Surface, which already stops
        // painting, so gpui never needs to see a Hidden window.
        WindowVisibility::Visible
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.ptr.state.borrow().background_appearance
    }

    fn set_title(&mut self, title: &str) {
        // Only routes for extra windows (each `ExtraWindowActivity` carries
        // OS chrome that displays the title). Primary GameActivity has no
        // chrome under our setup, so a setTitle there would be invisible.
        let state = self.ptr.state.borrow();
        let Some(window_id) = self.extra_window_id else {
            return;
        };
        let android_app = state.android_app.clone();
        drop(state);
        crate::multi_window::set_extra_activity_title(&android_app, window_id, title);
    }

    fn set_background_appearance(&self, _bg: WindowBackgroundAppearance) {}

    fn minimize(&self) {}
    fn zoom(&self) {}
    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.ptr.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.ptr.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.ptr.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_visibility_change(&self, _callback: Box<dyn FnMut(WindowVisibility)>) {}

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.ptr.callbacks.borrow_mut().hovered_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.ptr.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.ptr.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_display_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.ptr.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_hit_test_window_control(
        &self,
        _callback: Box<dyn FnMut() -> Option<WindowControlArea>>,
    ) {
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.ptr.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.ptr.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn set_drag_active(&self, active: bool) {
        self.ptr
            .state
            .borrow()
            .drag_active
            .store(active, std::sync::atomic::Ordering::Relaxed);
    }

    fn last_input_was_touch(&self) -> bool {
        self.ptr
            .state
            .borrow()
            .last_input_was_touch
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn toggle_soft_keyboard(&self) {
        let (android_app, extra_window_id) = {
            let state = self.ptr.state.borrow();
            (state.android_app.clone(), state.extra_window_id)
        };
        // Optimistically flip our local view of IME visibility
        // BEFORE the JNI roundtrip. The actual hide/show on Kotlin
        // side is async (runOnUiThread + IME animation), so Kotlin's
        // post-toggle `setImeShown` call can land 100-500ms after
        // the user tapped the button. Without the optimistic flip
        // the pane button's `toggle_state` lags by that much, the
        // user perceives "first tap didn't work, second tap did"
        // because by the time the visual catches up, they've already
        // tapped again. Kotlin's WindowInsetsListener will correct
        // if the OS-side toggle fails for any reason.
        let new_visible = !crate::ime::soft_keyboard_visible();
        crate::ime::SOFT_KEYBOARD_VISIBLE
            .store(new_visible, std::sync::atomic::Ordering::Release);
        crate::ime::toggle_keyboard(&android_app, extra_window_id);
    }

    fn soft_keyboard_visible(&self) -> bool {
        crate::ime::soft_keyboard_visible()
    }

    fn draw(&self, scene: &Scene) {
        let mut state = self.ptr.state.borrow_mut();
        let raw_window = state.raw_window;
        let Some(renderer) = state.renderer.as_mut() else {
            return;
        };

        if renderer.device_lost() {
            if raw_window.native_window.is_null() {
                log::warn!("draw: device lost but no native window to recover against");
                return;
            }
            match renderer.recover(&raw_window) {
                Ok(()) => {
                    state.force_render_after_recovery = true;
                }
                Err(err) => log::error!("GPU recovery failed: {err:#}"),
            }
            return;
        }

        renderer.draw(scene);
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        // `gpui::Window::new` calls this once during construction. `open_window`
        // blocks until `attach_surface` succeeds, so the renderer (and its atlas)
        // is always populated by the time gpui asks for it.
        self.ptr
            .state
            .borrow()
            .renderer
            .as_ref()
            .expect("sprite_atlas: open_window must attach surface before returning")
            .sprite_atlas()
            .clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.ptr
            .state
            .borrow()
            .renderer
            .as_ref()
            .and_then(|r| r.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}
}
