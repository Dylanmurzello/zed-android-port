//! Background survival of the rootfs engine's children: the
//! `android_runtime.keep_awake` setting feeding the runtime foreground
//! service, and the battery-optimization exemption Android asks the user
//! for in a system dialog. The service itself is driven from the process
//! tree in `gpui_android::runtime_service`.

use gpui::{App, Context, actions};
use settings::{Settings as _, SettingsStore};
use workspace::Workspace;

actions!(
    zdroid_runtime,
    [
        /// Ask Android to exempt Zdroid from battery optimization, so
        /// background language servers and terminals are not stopped.
        AllowBackgroundActivity,
    ]
);

pub fn register(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
            workspace.register_action(allow_background_activity);
        },
    )
    .detach();

    let push_keep_awake = |cx: &App| {
        let settings = workspace::AndroidRuntimeSettings::get_global(cx);
        gpui_android::runtime_service::set_keep_awake(settings.keep_awake);
    };
    push_keep_awake(cx);
    cx.observe_global::<SettingsStore>(move |cx| push_keep_awake(cx))
        .detach();
}

fn allow_background_activity(
    _workspace: &mut Workspace,
    _: &AllowBackgroundActivity,
    _window: &mut gpui::Window,
    _cx: &mut Context<Workspace>,
) {
    request_battery_exemption();
}

/// Ask once, on the launch that set the runtime up. The marker keeps the
/// dialog from returning after a reset; the settings page has the button
/// for later.
pub fn prompt_battery_exemption_once() {
    let marker = zdroid_runtime::data_path()
        .join("etc")
        .join("battery-exemption-asked");
    if marker.exists() {
        return;
    }
    match gpui_android::runtime_service::ignoring_battery_optimizations() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            log::warn!("runtime_background: battery optimization state unknown: {error:#}");
            return;
        }
    }
    if let Err(error) = std::fs::write(&marker, b"") {
        log::warn!(
            "runtime_background: cannot write {}: {error}",
            marker.display()
        );
    }
    request_battery_exemption();
}

fn request_battery_exemption() {
    match gpui_android::runtime_service::request_battery_exemption() {
        Ok(true) => {
            log::info!("runtime_background: battery exemption dialog opened or already granted")
        }
        Ok(false) => log::warn!("runtime_background: battery optimization settings unavailable"),
        Err(error) => log::warn!("runtime_background: battery exemption request failed: {error:#}"),
    }
}
