#![cfg(target_os = "android")]
//! Android backend for the GPUI Platform trait.

pub mod askpass_install;
mod captured_pointer;
mod clipboard;
mod cursor;
pub mod diagnostic;
mod dispatcher;
pub(crate) mod splash;
pub mod updater;
mod display;
pub mod dns_bridge;
mod events;
mod frame_timing;
mod ime;
mod keyboard;
mod multi_window;
mod platform;
mod saf;
pub mod storage;
pub mod termux_bootstrap;
mod touch;
mod window;
pub mod zd_exec_install;

pub use platform::AndroidPlatform;

/// Push the user's `android_input.on_screen_keyboard` setting into
/// the runtime atomic the IME reconcile loop reads when deciding
/// whether to auto-show the soft keyboard on text-input focus.
///
/// Decoupled from any `Window` because the gate must hold even when
/// no `Pane` has rendered yet (e.g. during onboarding before a
/// project is open — without a Pane render the per-render write
/// path in `workspace::pane` never fires and the atomic stays at
/// its `true` default, defeating the setting). Call this from a
/// `cx.observe_global::<SettingsStore>` hook in the app entry so
/// settings changes propagate regardless of which window is on
/// screen.
pub fn set_on_screen_keyboard_enabled(enabled: bool) {
    ime::ON_SCREEN_KEYBOARD_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
}

/// Push the user's effective trackpad-mode state (master AND active)
/// into the runtime atomic the touch dispatcher reads to decide
/// whether each touch event routes to the virtual-trackpad SM. Same
/// rationale as [`set_on_screen_keyboard_enabled`]: must work
/// independent of pane render so the gate is correct before any
/// project is opened.
pub fn set_trackpad_mode_enabled(enabled: bool) {
    ime::TRACKPAD_MODE_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
}

/// Push the user's `android_input.programming_extras_row` setting
/// to the runtime atomic. The platform reconcile loop forwards
/// changes to Kotlin's `Activity.setProgrammingExtrasRowEnabled` so
/// the extras-row view is inflated / torn down without restart.
pub fn set_programming_extras_row_enabled(enabled: bool) {
    ime::EXTRAS_ROW_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);
}

/// Read the current value of the extras-row atomic. Used by the
/// platform reconcile loop to drive the JNI push to Kotlin only on
/// transitions.
pub(crate) fn programming_extras_row_enabled() -> bool {
    ime::EXTRAS_ROW_ENABLED.load(std::sync::atomic::Ordering::Acquire)
}

use std::rc::Rc;

/// Run a gpui application backed by Android's GameActivity event loop.
///
/// Call this from your `android_main` no_mangle entry point. It constructs
/// the `AndroidPlatform`, hands it to `gpui::Application`, and drives the
/// run loop. Replaces the desktop/web `gpui_platform::application()` path
/// because we need to thread the `AndroidApp` through Platform construction.
///
/// `assets` is the source the SVG renderer uses to resolve `icons/...`,
/// `images/...`, and any other asset paths Zed's UI references. Pass
/// `assets::Assets` (the bundled RustEmbed source) for the typical case;
/// without an asset source icons render as blank rectangles.
pub fn run<A, F>(android_app: android_activity::AndroidApp, assets: A, on_finish_launching: F)
where
    A: gpui::AssetSource,
    F: 'static + FnOnce(&mut gpui::App),
{
    // Stash a clone for the updater module so action handlers / auto-
    // check tasks (which run inside the gpui App context with no
    // direct access to the AndroidApp passed into this function) can
    // get back to JNI calls. Set once per process — idempotent across
    // Activity recreations.
    updater::register_android_app(android_app.clone());
    diagnostic::register_android_app(android_app.clone());
    let platform: Rc<dyn gpui::Platform> = Rc::new(AndroidPlatform::new(android_app, false));
    let app = gpui::Application::with_platform(platform).with_assets(assets);
    app.run(on_finish_launching);
}
