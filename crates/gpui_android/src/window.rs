use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;

use android_activity::AndroidApp;
use anyhow::Result;
use futures::channel::oneshot;
use ndk::native_window::{FrameRateCompatibility, NativeWindow};
use raw_window_handle as rwh;

use gpui::{
    AnyWindowHandle, Bounds, Capslock, Corners, DevicePixels, DispatchEventResult, Edges,
    FrameRequestSource, GestureTuning, GpuSpecs, Modifiers, Pixels, PlatformAtlas, PlatformDisplay,
    PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel,
    RequestFrameOptions, Scene, Size, TextInputStateChange, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowInsets, WindowParams,
    WindowVisibility, point, px, size,
};
use gpui_wgpu::{GpuContext, WgpuRenderer, WgpuSurfaceConfig, wgpu};

use crate::display::AndroidDisplay;
use crate::insets::PhysicalInsets;
use crate::platform::{frame_presented, request_frame_callback};

const TARGET_FRAME_RATE_HZ: f32 = 120.0;

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
    pub(crate) visibility_change: Option<Box<dyn FnMut(WindowVisibility)>>,
    pub(crate) insets_changed: Option<Box<dyn FnMut(WindowInsets)>>,
}

pub(crate) struct AndroidWindowState {
    /// The whole surface. gpui lays out in [`Self::layout_bounds`].
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) scale_factor: f32,
    pub(crate) insets: PhysicalInsets,
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
    /// The next paint must redraw everything, because of something gpui's
    /// invalidator doesn't know about: a fresh swapchain, a cleared atlas, state
    /// of the status bar's keyboard button. Consumed by `refresh()`.
    pub(crate) force_render: bool,
    /// Set when a present failed, so the next frame presents the last scene
    /// again; consumed by `refresh()`.
    pub(crate) present_on_next_frame: bool,
    /// Whether the last `draw` failed to present, so a second failure in a
    /// row isn't retried.
    pub(crate) last_present_failed: bool,
    /// The window's `ExtraWindowActivity` is already gone (the user closed it
    /// through the OS chrome), so `AndroidWindow::Drop` needn't finish it.
    pub(crate) os_closed: bool,
    /// Set when gpui closes an extra window and drops its `AndroidWindow`.
    /// The platform run loop then drops the window from `extra_windows`.
    pub(crate) closed: bool,
    /// Hidden while the window has no surface (the app or this Activity is
    /// in the background, or the screen is off): gpui would lay out and
    /// paint frames that can't be presented.
    pub(crate) visibility: WindowVisibility,
    /// The visibility last reported to gpui, which can't be called back
    /// while it opens a window; see [`AndroidWindowStatePtr::sync_visibility`].
    pub(crate) reported_visibility: WindowVisibility,
    /// Whether the window's Activity has input focus. Starts true because
    /// each window opens in front. Zed closes the active window for
    /// `workspace::CloseWindow`, so with every window reporting active,
    /// Ctrl+W in Settings closed the main window instead, and Zed quit.
    pub(crate) active: bool,
    /// The pointer state from the last input, recorded the way gpui records
    /// it. gpui reads these back whenever the window is activated or
    /// resized, so defaults would move its pointer to the top-left corner.
    pub(crate) mouse_position: Point<Pixels>,
    pub(crate) modifiers: Modifiers,
    pub(crate) capslock: Capslock,
    /// Cheap clone (Arc internally) used by `AndroidWindow::Drop` to issue
    /// `multi_window::finish_extra_activity` for extra windows. Always set
    /// at construction time (both primary and extras carry it; primary just
    /// never reaches the Drop branch that uses it).
    pub(crate) android_app: AndroidApp,
    /// The held button and the run of clicks, per window so that a click in
    /// one window doesn't continue a double click in another.
    pub(crate) mouse: crate::events::mouse::MouseState,
    pub(crate) trackpad_scroll: crate::events::mouse::TrackpadScroll,
    /// Feeds finger gestures to gpui's own touch gesture recognizer. Each
    /// window's touchscreen events are isolated.
    pub(crate) touch_feed: crate::touch::TouchFeed,
    /// `None` for the main window, `Some(id)` for one that an
    /// `ExtraWindowActivity` hosts.
    pub(crate) extra_window_id: Option<u64>,
    /// The IME composition, tracked here rather than through
    /// `handler.marked_text_range()`, whose support varies by input view:
    /// the editor implements it, the terminal effectively doesn't, and trusting
    /// it typed duplicate letters and lost text on Enter. Its start is the
    /// UTF-16 index where the first `setComposingText` landed; each new one
    /// replaces `start..start + last_text.utf16_len()`. Cleared on commit,
    /// finish or cancel. So the composition is visibly committed as it's typed,
    /// with no underline.
    pub(crate) ime_composition_start: Option<usize>,
    pub(crate) ime_composition_text: Option<String>,
    /// The accent a dead key on a physical keyboard is showing as a composition,
    /// for the next key to combine with.
    pub(crate) pending_accent: Option<char>,
    /// Per-window mirror of whether the input handler was present at
    /// the last frame-boundary IME reconcile. Compared against the
    /// current `input_handler.is_some()` to detect focus gain and
    /// loss. Per-window so that an `ExtraWindowActivity` (settings,
    /// picker) can drive its own IME independent of the primary
    /// surface.
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

impl AndroidWindowState {
    /// The part of the surface above the on-screen keyboard and the
    /// programming extras row on it, where gpui lays out. Edge-to-edge
    /// windows don't get `adjustResize`, so this shrinks the layout the way
    /// it shrinks native views. The surface keeps its size: gpui draws from
    /// the top-left corner, and the keyboard and the row cover the rest.
    pub(crate) fn layout_bounds(&self) -> Bounds<Pixels> {
        let obscured_height = self.to_logical(self.insets.obscured_bottom);
        let mut bounds = self.bounds;
        bounds.size.height = (bounds.size.height - obscured_height).max(Pixels::ZERO);
        bounds
    }

    fn to_logical(&self, physical: i32) -> Pixels {
        px(physical as f32 / self.scale_factor)
    }

    fn window_insets(&self) -> WindowInsets {
        let insets = self.insets;
        WindowInsets {
            safe_area: Edges {
                top: self.to_logical(insets.safe_area_top),
                right: self.to_logical(insets.safe_area_right),
                // Measured from the layout area's bottom edge, which is the
                // keyboard's or the extras row's top edge while it's up.
                bottom: self.to_logical((insets.safe_area_bottom - insets.obscured_bottom).max(0)),
                left: self.to_logical(insets.safe_area_left),
            },
            // Consumed, as `adjustResize` consumes it: the layout area
            // already ends above the keyboard.
            ime: Edges::default(),
            corner_radii: Corners::default(),
        }
    }
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
            // Android's Vulkan surfaces only offer the inherit alpha mode, and
            // the SurfaceViews' opaque pixel formats decide the compositing.
            transparent: true,
            // Mailbox lets the swap chain discard a stale frame at present
            // time when a newer one is ready, which under irregular paint
            // cadence (scroll bursts, typing flurries) feels ~1 frame
            // tighter than FIFO. Falls back to Fifo inside the renderer
            // if the surface doesn't expose Mailbox.
            //
            // More swapchain images added 16-33 ms of input latency whenever
            // Samsung's adaptive panel had dropped to 60 or 30 Hz; see
            // `docs/workarounds/deferred-render-pipeline-perf.md`.
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

        let gpu_context = state.gpu_context.clone();
        if let Some(renderer) = state.renderer.as_mut() {
            let instance = gpu_context
                .borrow()
                .as_ref()
                .map(|ctx| ctx.instance.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("attach_surface: gpu_context missing on re-attach")
                })?;
            renderer.replace_surface(&raw_window, config, &instance)?;
            log::info!("AndroidWindow::attach_surface: replaced surface ({width}x{height})");
        } else {
            let renderer = WgpuRenderer::new(gpu_context, &raw_window, config, None)?;
            state.renderer = Some(renderer);
            log::info!("AndroidWindow::attach_surface: created renderer ({width}x{height})");
        }

        // Asks for the panel's top refresh rate on each attach: Android
        // otherwise leaves the window at 60 Hz. Rates the panel doesn't have
        // settle at the nearest one. Passing 0 would mean "no preference", which
        // Samsung's adaptive panel reads as a cue to drop to its idle rate.
        if let Err(error) =
            native_window.set_frame_rate(TARGET_FRAME_RATE_HZ, FrameRateCompatibility::Default)
        {
            log::warn!("ANativeWindow_setFrameRate failed: {error}");
        }

        // Replace the previous wrapper (if any). The drop releases the prior
        // ANativeWindow refcount; Vulkan's VkSurfaceKHR holds its own ref.
        state.native_window = Some(native_window);
        state.visibility = WindowVisibility::Visible;

        // Force a paint on the next refresh tick. After a fresh `replace_surface`
        // the swapchain is uninitialized (presents black), and gpui's
        // request_frame is a no-op when the invalidator hasn't been dirtied.
        // On first attach this is a no-op duplicate of gpui's own initial
        // `window.draw(cx)` inside open_window, but it's cheap and makes the
        // background→foreground path symmetric.
        state.force_render = true;
        request_frame_callback();
        Ok(())
    }

    /// Whether frames drawn now can be presented.
    pub(crate) fn is_visible(&self) -> bool {
        self.state.borrow().visibility.is_visible()
    }

    /// Reports a visibility change to gpui. The run loop calls this rather
    /// than `attach_surface`/`detach_surface`, which also run while gpui is
    /// opening a window and can't take callbacks.
    pub(crate) fn sync_visibility(&self) {
        let visibility = {
            let mut state = self.state.borrow_mut();
            if state.visibility == state.reported_visibility {
                return;
            }
            state.reported_visibility = state.visibility;
            state.visibility
        };
        let callback = self.callbacks.borrow_mut().visibility_change.take();
        if let Some(mut callback) = callback {
            callback(visibility);
            self.callbacks.borrow_mut().visibility_change = Some(callback);
        }
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
        state.visibility = WindowVisibility::Hidden;
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
        {
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
        }
        self.notify_resize();
    }

    /// Records the insets the window's Activity reported. While the keyboard
    /// slides, each frame resizes gpui's layout area, like a live window
    /// resize; the surface stays as it is.
    pub(crate) fn set_insets(&self, insets: PhysicalInsets) {
        let (previous, previous_layout_size, layout_size, extra_window_id) = {
            let mut state = self.state.borrow_mut();
            if state.insets == insets {
                return;
            }
            let previous_layout_size = state.layout_bounds().size;
            let previous = std::mem::replace(&mut state.insets, insets);
            // gpui has already removed a closed window, so its callbacks
            // would only fail with "window not found".
            if state.closed {
                return;
            }
            (
                previous,
                previous_layout_size,
                state.layout_bounds().size,
                state.extra_window_id,
            )
        };
        // Every frame of the keyboard's slide lands here, so only its start
        // and end are logged at info.
        let obscured_toggled = (previous.obscured_bottom > 0) != (insets.obscured_bottom > 0);
        let safe_area_changed = PhysicalInsets {
            obscured_bottom: insets.obscured_bottom,
            ..previous
        } != insets;
        if obscured_toggled || safe_area_changed {
            log::info!(
                "AndroidWindow(extra={extra_window_id:?}): {insets:?}, layout size {layout_size:?}"
            );
        } else {
            log::debug!(
                "AndroidWindow(extra={extra_window_id:?}): {insets:?}, layout size {layout_size:?}"
            );
        }
        if layout_size != previous_layout_size {
            self.notify_resize();
        }
        let callback = self.callbacks.borrow_mut().insets_changed.take();
        if let Some(mut callback) = callback {
            let window_insets = self.state.borrow().window_insets();
            callback(window_insets);
            self.callbacks.borrow_mut().insets_changed = Some(callback);
        }
    }

    /// Tells gpui to lay out again at [`AndroidWindowState::layout_bounds`].
    fn notify_resize(&self) {
        let (layout_size, scale_factor) = {
            let state = self.state.borrow();
            (state.layout_bounds().size, state.scale_factor)
        };
        let callback = self.callbacks.borrow_mut().resize.take();
        if let Some(mut callback) = callback {
            callback(layout_size, scale_factor);
            self.callbacks.borrow_mut().resize = Some(callback);
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
        let (force_render, require_presentation) = {
            let mut state = self.state.borrow_mut();
            (
                std::mem::take(&mut state.force_render),
                std::mem::take(&mut state.present_on_next_frame),
            )
        };
        let callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation,
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

    /// Records the window's Activity gaining or losing input focus and tells
    /// gpui when that changes.
    pub(crate) fn set_active(&self, active: bool) {
        let was_active = {
            let mut state = self.state.borrow_mut();
            // gpui has already removed a closed window, so its callback
            // would only fail with "window not found".
            if state.closed {
                return;
            }
            std::mem::replace(&mut state.active, active)
        };
        if was_active != active {
            self.notify_active_status_change(active);
        }
    }

    /// Dispatches a translated input event into gpui via the registered
    /// `on_input` callback, then routes printable `KeyDown`s through the
    /// active `PlatformInputHandler` (gpui's text-input path) when the
    /// callback didn't claim them.
    pub(crate) fn handle_input(&self, input: PlatformInput) {
        // Android repeats a held modifier key like any other, and gpui expects
        // only real changes, as `last_reported_modifiers` in `gpui_windows`
        // ensures. Zed's editor drops the link under a pressed button when told
        // of one, so Ctrl+click did nothing with a mouse.
        if let PlatformInput::ModifiersChanged(event) = &input {
            let state = self.state.borrow();
            if state.modifiers == event.modifiers && state.capslock == event.capslock {
                return;
            }
        }
        self.record_pointer_state(&input);
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
            if event.keystroke.modifiers.is_subset_of(&Modifiers::shift())
                && let Some(key_char) = &event.keystroke.key_char
            {
                self.with_input_handler(|handler| handler.replace_text_in_range(None, key_char));
            }
        }
    }

    /// Runs `f` with the window's input handler, if it has one, with the
    /// window's state free. The handler's calls update gpui, whose effects
    /// reach back into this window: setting its title, say, as an edit makes
    /// the buffer dirty.
    pub(crate) fn with_input_handler<T>(
        &self,
        f: impl FnOnce(&mut PlatformInputHandler) -> T,
    ) -> Option<T> {
        let mut handler = self.state.borrow_mut().input_handler.take()?;
        let result = f(&mut handler);
        self.state.borrow_mut().input_handler = Some(handler);
        Some(result)
    }

    /// Mirrors `Window::dispatch_event`'s bookkeeping.
    fn record_pointer_state(&self, input: &PlatformInput) {
        let mut state = self.state.borrow_mut();
        match input {
            PlatformInput::MouseMove(event) => {
                state.mouse_position = event.position;
                state.modifiers = event.modifiers;
            }
            PlatformInput::MouseDown(event) => {
                state.mouse_position = event.position;
                state.modifiers = event.modifiers;
            }
            PlatformInput::MouseUp(event) => {
                state.mouse_position = event.position;
                state.modifiers = event.modifiers;
            }
            PlatformInput::MouseExited(event) => {
                state.modifiers = event.modifiers;
            }
            PlatformInput::ModifiersChanged(event) => {
                state.modifiers = event.modifiers;
                state.capslock = event.capslock;
            }
            PlatformInput::Scroll(event) => {
                state.mouse_position = event.position;
                state.modifiers = event.modifiers;
            }
            PlatformInput::Pinch(event) => {
                state.mouse_position = event.position;
                state.modifiers = event.modifiers;
            }
            _ => {}
        }
    }
}

pub(crate) struct AndroidWindow {
    pub(crate) ptr: AndroidWindowStatePtr,
}

impl AndroidWindow {
    /// An explicit request to show or hide the soft keyboard. A show does
    /// nothing with `android_input.on_screen_keyboard` off or a physical
    /// keyboard blocking the on-screen one, when the status bar hides its
    /// keyboard button: One UI's Samsung Keyboard doesn't show then anyway.
    fn request_soft_keyboard(&self, show: bool) {
        if show
            && (!crate::ime::on_screen_keyboard_enabled()
                || crate::ime::physical_keyboard_blocks_ime())
        {
            return;
        }
        let (android_app, extra_window_id) = {
            let state = self.ptr.state.borrow();
            (state.android_app.clone(), state.extra_window_id)
        };
        // Set our view of the keyboard before the asynchronous JNI call: a
        // button showing the old state until the keyboard moves reads as a
        // missed tap. `WindowIme.kt` corrects it if the request does nothing.
        crate::ime::SOFT_KEYBOARD_VISIBLE.store(show, std::sync::atomic::Ordering::Release);
        crate::ime::request_keyboard(&android_app, extra_window_id, show);
    }

    pub(crate) fn new(
        handle: AnyWindowHandle,
        _params: WindowParams,
        gpu_context: GpuContext,
        appearance: WindowAppearance,
        android_app: AndroidApp,
        tuning: GestureTuning,
    ) -> Self {
        let display: Rc<dyn PlatformDisplay> = Rc::new(AndroidDisplay::new());
        let bounds = display.bounds();

        let state = AndroidWindowState {
            bounds,
            scale_factor: 1.0,
            insets: PhysicalInsets::default(),
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
            force_render: false,
            present_on_next_frame: false,
            last_present_failed: false,
            os_closed: false,
            closed: false,
            visibility: WindowVisibility::Visible,
            reported_visibility: WindowVisibility::Visible,
            active: true,
            mouse_position: Point::default(),
            modifiers: Modifiers::default(),
            capslock: Capslock::default(),
            android_app,
            mouse: crate::events::mouse::MouseState::default(),
            trackpad_scroll: crate::events::mouse::TrackpadScroll::new(tuning),
            touch_feed: crate::touch::TouchFeed::new(tuning.touch_slop),
            extra_window_id: None,
            ime_composition_start: None,
            ime_composition_text: None,
            pending_accent: None,
            ime_currently_visible: false,
            last_ime_target_kind: None,
            last_pushed_selection: None,
        };

        Self {
            ptr: AndroidWindowStatePtr {
                state: Rc::new(RefCell::new(state)),
                callbacks: Rc::new(RefCell::new(Callbacks::default())),
            },
        }
    }

    pub(crate) fn ptr(&self) -> AndroidWindowStatePtr {
        self.ptr.clone()
    }
}

impl Drop for AndroidWindow {
    fn drop(&mut self) {
        let mut state = self.ptr.state.borrow_mut();
        // The main window's Activity outlives it.
        let Some(window_id) = state.extra_window_id else {
            return;
        };
        state.closed = true;
        if !state.os_closed {
            let android_app = state.android_app.clone();
            drop(state);
            crate::multi_window::finish_extra_activity(&android_app, window_id);
        }
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
        self.ptr.state.borrow().layout_bounds()
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        // Zed saves this to restore the window, which shouldn't depend on
        // whether the keyboard was up.
        WindowBounds::Maximized(self.ptr.state.borrow().bounds)
    }

    fn content_size(&self) -> Size<Pixels> {
        self.ptr.state.borrow().layout_bounds().size
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
        self.ptr.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.ptr.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.ptr.state.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        // No keyboard call here — gpui's paint cycle calls
        // take_input_handler/set_input_handler every frame for state
        // shuffling (window.rs line 2497-2509), which would oscillate
        // the IME visibly. Hiding on focus loss is driven by a
        // frame-boundary diff in `AndroidPlatform::reconcile_ime_visibility`
        // instead.
        self.ptr.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
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
        let (extra_window_id, android_app) = {
            let state = self.ptr.state.borrow();
            (state.extra_window_id, state.android_app.clone())
        };
        if let Some(window_id) = extra_window_id {
            crate::multi_window::activate_extra_activity(&android_app, window_id);
        }
    }

    fn is_active(&self) -> bool {
        self.ptr.state.borrow().active
    }

    fn visibility(&self) -> WindowVisibility {
        self.ptr.state.borrow().reported_visibility
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.ptr.state.borrow().background_appearance
    }

    fn set_title(&mut self, title: &str) {
        // Only an extra window has OS chrome to show it.
        let (extra_window_id, android_app) = {
            let state = self.ptr.state.borrow();
            (state.extra_window_id, state.android_app.clone())
        };
        if let Some(window_id) = extra_window_id {
            crate::multi_window::set_extra_activity_title(&android_app, window_id, title);
        }
    }

    fn set_background_appearance(&self, _bg: WindowBackgroundAppearance) {}

    fn minimize(&self) {}
    fn zoom(&self) {}
    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        Some(Rc::new(request_frame_callback))
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

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.ptr.callbacks.borrow_mut().visibility_change = Some(callback);
    }

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

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.ptr.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.ptr.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn insets(&self) -> WindowInsets {
        self.ptr.state.borrow().window_insets()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.ptr.callbacks.borrow_mut().insets_changed = Some(callback);
    }

    fn show_soft_keyboard(&self) {
        self.request_soft_keyboard(true);
    }

    fn hide_soft_keyboard(&self) {
        self.request_soft_keyboard(false);
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        let ime_window_id = self.ptr.state.borrow().extra_window_id.unwrap_or(0);
        match change {
            TextInputStateChange::FocusGained => {
                crate::ime::set_text_input_focused(ime_window_id, true)
            }
            TextInputStateChange::FocusLost => {
                crate::ime::set_text_input_focused(ime_window_id, false)
            }
            // Zed's editor doesn't report these yet; the run loop polls the
            // selection while the keyboard is up.
            TextInputStateChange::SelectionChanged | TextInputStateChange::ContentChanged => {}
        }
    }

    fn draw(&self, scene: &Scene) {
        frame_presented();
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
                    state.force_render = true;
                    request_frame_callback();
                }
                Err(err) => log::error!("GPU recovery failed: {err:#}"),
            }
            return;
        }

        let presented = renderer.draw(scene);
        // gpui knows about neither of these, and its Wayland backend handles
        // them the same way. When the renderer cleared its atlas, cached
        // views still point into it, so the next frame redraws everything.
        // When presenting failed, for example on an out-of-date swapchain
        // the renderer just reconfigured, the next frame presents the same
        // scene again; a failure that repeats, as while the surface is
        // unconfigured, waits for the next change.
        if renderer.needs_redraw() {
            state.force_render = true;
            request_frame_callback();
        } else if !presented && !state.last_present_failed {
            log::debug!("draw: present failed, presenting again on the next frame");
            state.present_on_next_frame = true;
            request_frame_callback();
        }
        state.last_present_failed = !presented;
    }

    fn schedule_frame(&self) {
        request_frame_callback();
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
