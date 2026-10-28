//! Zdroid's Android page, in its own file so that `page_data.rs`, which
//! changes with most Zed releases, only gains the line adding it.

use std::sync::Arc;

use crate::{ActionLink, SettingField, SettingItem, SettingsPage, SettingsPageItem, USER};

pub(super) fn android_page() -> SettingsPage {
    SettingsPage {
        title: "Android",
        items: Box::new([
            SettingsPageItem::SectionHeader("Runtime"),
            SettingsPageItem::ActionLink(ActionLink {
                title: "Pick runtime".into(),
                description: Some(
                    "Choose the userland Zdroid runs commands in: Bootstrap, a \
                     Termux-derived userland, or Kali chroot. The terminal, \
                     language servers, formatters and other tools run there."
                        .into(),
                ),
                button_text: "Open picker".into(),
                // The app registers `zdroid_runtime::PickRuntime` on workspaces,
                // and its handler opens the picker as a window of its own.
                on_click: Arc::new(|_settings_window, _window, cx| {
                    workspace::with_active_or_new_workspace(cx, |_workspace, window, cx| match cx
                        .build_action("zdroid_runtime::PickRuntime", None)
                    {
                        Ok(action) => window.dispatch_action(action, cx),
                        Err(error) => log::warn!(
                            "settings_ui: zdroid_runtime::PickRuntime isn't registered: {error}"
                        ),
                    });
                }),
                files: USER,
            }),
            SettingsPageItem::SectionHeader("On-Screen Keyboard"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Enable On-Screen Keyboard",
                description: "Auto-show the soft IME on text-input \
                    focus and expose its toggle in the pane tab bar. \
                    Turn off when using a hardware keyboard.",
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
                description: "Render a row above the soft keyboard \
                    with Esc, Tab, Ctrl, Alt, and arrow keys. Ctrl \
                    and Alt are sticky: tap once for next key, \
                    double-tap to lock until tapped again.",
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
            SettingsPageItem::SectionHeader("Touch Behavior"),
            SettingsPageItem::SettingItem(SettingItem {
                title: "Enable Virtual Trackpad",
                description: "Add a status bar toggle that switches \
                    the screen into VNC-style trackpad mode: drag \
                    moves an on-screen cursor, tap clicks, \
                    two-finger tap is right-click, two-finger drag \
                    scrolls. While trackpad mode is active, standard \
                    Android touch (direct-tap on UI elements, scroll, \
                    splitter drag) is disabled; toggle off from the \
                    status bar icon to go back.",
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
                description: "Reverse the scroll direction of the \
                    trackpad (two-finger) and the mouse wheel \
                    (macOS-style natural vs traditional). Affects the \
                    hardware trackpad, the mouse wheel, and the virtual \
                    trackpad. Direct-touch finger scrolling is never \
                    inverted.",
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
