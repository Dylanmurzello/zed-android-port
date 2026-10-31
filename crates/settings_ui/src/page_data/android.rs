//! Zdroid's Android page, in its own file so that `page_data.rs`, which
//! changes with most Zed releases, only gains the line adding it.

use std::sync::Arc;

use gpui::App;

use crate::{ActionLink, SettingField, SettingItem, SettingsPage, SettingsPageItem, USER};

/// Dispatches an action the app registers on workspaces. The Settings window
/// has no workspace, so the action goes to the active one.
fn dispatch_app_action(name: &'static str, cx: &mut App) {
    workspace::with_active_or_new_workspace(cx, move |_workspace, window, cx| {
        match cx.build_action(name, None) {
            Ok(action) => window.dispatch_action(action, cx),
            Err(error) => log::warn!("settings_ui: {name} isn't registered: {error}"),
        }
    });
}

pub(super) fn android_page() -> SettingsPage {
    SettingsPage {
        title: "Android",
        items: Box::new([
            SettingsPageItem::SectionHeader("Runtime"),
            SettingsPageItem::ActionLink(ActionLink {
                title: "Reset Runtime".into(),
                description: Some(
                    "Extract the bundled Ubuntu image again. Removes what you installed with \
                     apt; your home folder, projects and settings stay."
                        .into(),
                ),
                button_text: "Reset".into(),
                on_click: Arc::new(|_settings_window, _window, cx| {
                    dispatch_app_action("zdroid_runtime::ResetRuntime", cx);
                }),
                files: USER,
            }),
            SettingsPageItem::ActionLink(ActionLink {
                title: "Install Essentials".into(),
                description: Some(
                    "Install git, the SSH client and the CA certificates with apt, which the \
                     base image lacks. This runs on first launch; use it if that failed \
                     without a network, or after a reset."
                        .into(),
                ),
                button_text: "Install".into(),
                on_click: Arc::new(|_settings_window, _window, cx| {
                    dispatch_app_action("zdroid_runtime::InstallEssentials", cx);
                }),
                files: USER,
            }),
            SettingsPageItem::SectionHeader("Storage and Background"),
            SettingsPageItem::ActionLink(ActionLink {
                title: "Allow Access to /sdcard".into(),
                description: Some(
                    "Open Android's All files access toggle, which projects outside the app's \
                     own folder need. Zdroid asks once on first launch."
                        .into(),
                ),
                button_text: "Open".into(),
                on_click: Arc::new(|_settings_window, _window, cx| {
                    dispatch_app_action("zdroid_runtime::AllowAllFilesAccess", cx);
                }),
                files: USER,
            }),
            SettingsPageItem::ActionLink(ActionLink {
                title: "Allow Background Activity".into(),
                description: Some(
                    "Ask Android to exempt Zdroid from battery optimization, so language \
                     servers and terminals keep running in the background."
                        .into(),
                ),
                button_text: "Request".into(),
                on_click: Arc::new(|_settings_window, _window, cx| {
                    dispatch_app_action("zdroid_runtime::AllowBackgroundActivity", cx);
                }),
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Keep Awake",
                description: "Hold a wake lock while language servers or terminals run, so builds and indexing continue with the screen off. Costs battery.",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("android_runtime.keep_awake"),
                    pick: |settings_content| {
                        settings_content
                            .android_runtime
                            .as_ref()?
                            .keep_awake
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .android_runtime
                            .get_or_insert_default()
                            .keep_awake = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SectionHeader("On-Screen Keyboard"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Enable On-Screen Keyboard",
                description: "Show the on-screen keyboard when a text field gets focus, and its button in the status bar. Turn off when using a hardware keyboard.",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("android_input.on_screen_keyboard"),
                    pick: |settings_content| {
                        settings_content
                            .android_input
                            .as_ref()?
                            .on_screen_keyboard
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .android_input
                            .get_or_insert_default()
                            .on_screen_keyboard = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Show Programming Keys Row",
                description: "Show Esc, Tab, Ctrl, Alt and arrow keys above the on-screen keyboard. Tap Ctrl or Alt once for the next key, or twice to lock it.",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("android_input.programming_extras_row"),
                    pick: |settings_content| {
                        settings_content
                            .android_input
                            .as_ref()?
                            .programming_extras_row
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .android_input
                            .get_or_insert_default()
                            .programming_extras_row = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SectionHeader("Trackpad and Mouse"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Enable Virtual Trackpad",
                description: "Add a status bar button that turns the screen into a trackpad: drag moves the pointer, tap clicks, two-finger tap right-clicks and two-finger drag scrolls. Tapping the button again restores direct touch.",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("android_input.trackpad_mode"),
                    pick: |settings_content| {
                        settings_content
                            .android_input
                            .as_ref()?
                            .trackpad_mode
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .android_input
                            .get_or_insert_default()
                            .trackpad_mode = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Invert Scroll Direction",
                description: "Reverse the scroll direction of trackpads, the mouse wheel and the virtual trackpad. Scrolling with a finger isn't affected.",
                field: Box::new(SettingField {
                    organization_override: None,
                    json_path: Some("android_input.invert_scroll"),
                    pick: |settings_content| {
                        settings_content
                            .android_input
                            .as_ref()?
                            .invert_scroll
                            .as_ref()
                    },
                    write: |settings_content, value, _| {
                        settings_content
                            .android_input
                            .get_or_insert_default()
                            .invert_scroll = value;
                    },
                }),
                metadata: None,
                files: USER,
            }),
        ]),
    }
}
