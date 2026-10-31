//! Background survival of the rootfs engine's children: the
//! `android_runtime.keep_awake` setting feeding the runtime foreground
//! service, and the battery-optimization exemption Android asks the user
//! for in a system dialog. The service itself is driven from the process
//! tree in `gpui_android::runtime_service`. The All files access toggle
//! for `/sdcard` lives here too, as the other system screen the settings
//! page opens.

use gpui::{App, Context, UpdateGlobal as _, actions};
use settings::{
    AndroidRuntimeSettingsContent, RegisterSetting, Settings, SettingsContent, SettingsStore,
};
use workspace::Workspace;

#[derive(RegisterSetting)]
pub struct AndroidRuntimeSettings {
    pub keep_awake: bool,
}

impl Settings for AndroidRuntimeSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let android_runtime = content.android_runtime.clone().unwrap_or_default();
        AndroidRuntimeSettings {
            keep_awake: android_runtime.keep_awake.unwrap_or(false),
        }
    }
}

/// Adds the `android_runtime` defaults to Zed's default settings, which only
/// Android reads, so the Settings window shows their values.
pub fn set_default_settings(cx: &mut App) {
    SettingsStore::update_global(cx, |store, cx| {
        store.update_default_settings(cx, |defaults| {
            defaults.android_runtime = Some(AndroidRuntimeSettingsContent {
                keep_awake: Some(false),
            });
        });
    });
}

actions!(
    zdroid_runtime,
    [
        /// Ask Android to exempt Zdroid from battery optimization, so
        /// background language servers and terminals are not stopped.
        AllowBackgroundActivity,
        /// Open the system toggle that lets Zdroid read and write files
        /// under /sdcard (All files access on Android 11 and later).
        AllowAllFilesAccess,
    ]
);

pub fn register(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
            workspace.register_action(allow_background_activity);
            workspace.register_action(allow_all_files_access);
        },
    )
    .detach();

    let push_keep_awake = |cx: &App| {
        let settings = AndroidRuntimeSettings::get_global(cx);
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

fn allow_all_files_access(
    _workspace: &mut Workspace,
    _: &AllowAllFilesAccess,
    _window: &mut gpui::Window,
    _cx: &mut Context<Workspace>,
) {
    match gpui_android::storage::request_all_files_access() {
        Ok(true) => {
            log::info!("runtime_background: all files access screen opened or already granted")
        }
        Ok(false) => log::warn!("runtime_background: all files access settings unavailable"),
        Err(error) => log::warn!("runtime_background: all files access request failed: {error:#}"),
    }
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
