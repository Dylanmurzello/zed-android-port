//! Android input: the `android_input` settings, their status bar buttons
//! (show or hide the on-screen keyboard, switch the screen between direct
//! touch and a virtual trackpad) and their onboarding section.

use fs::Fs;
use gpui::{
    AnyElement, App, Context, IntoElement, Render, Subscription, UpdateGlobal as _, Window, div,
};
use settings::{
    AndroidInputSettingsContent, RegisterSetting, Settings, SettingsContent, SettingsStore,
    ShowCloseButton,
};
use ui::{IconButton, IconName, IconSize, Switch, Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

#[derive(RegisterSetting)]
pub struct AndroidInputSettings {
    pub on_screen_keyboard: bool,
    pub trackpad_mode: bool,
    pub trackpad_mode_active: bool,
    pub programming_extras_row: bool,
    pub invert_scroll: bool,
}

impl Settings for AndroidInputSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let android_input = content.android_input.clone().unwrap_or_default();
        AndroidInputSettings {
            on_screen_keyboard: android_input.on_screen_keyboard.unwrap_or(true),
            trackpad_mode: android_input.trackpad_mode.unwrap_or(false),
            trackpad_mode_active: android_input.trackpad_mode_active.unwrap_or(false),
            programming_extras_row: android_input.programming_extras_row.unwrap_or(false),
            invert_scroll: android_input.invert_scroll.unwrap_or(false),
        }
    }
}

/// Adds the `android_input` defaults to Zed's default settings, which only
/// Android reads, so the Settings window shows their values. Also shows tabs'
/// close buttons, which Zed shows on hover, since a finger doesn't hover.
pub fn set_default_settings(cx: &mut App) {
    SettingsStore::update_global(cx, |store, cx| {
        store.update_default_settings(cx, |defaults| {
            defaults.android_input = Some(AndroidInputSettingsContent {
                on_screen_keyboard: Some(true),
                trackpad_mode: Some(false),
                trackpad_mode_active: Some(false),
                programming_extras_row: Some(false),
                invert_scroll: Some(false),
            });
            defaults.tabs.get_or_insert_default().show_close_button = Some(ShowCloseButton::Always);
        });
    });
}

/// The onboarding page's input section (`onboarding::AndroidBasicsSections`).
/// Each feature is a row with a `Switch`: a `ToggleButtonGroup` of tiles,
/// as Base Keymap uses, read as "which one is picked" rather than "on or off".
pub fn render_onboarding_section(tab_index: &mut isize, cx: &mut App) -> AnyElement {
    let settings = AndroidInputSettings::get_global(cx);
    let rows = [
        onboarding_row(
            "onboarding-android-on-screen-keyboard",
            IconName::Keyboard,
            "Soft Keyboard",
            settings.on_screen_keyboard,
            |content, value| content.on_screen_keyboard = Some(value),
            tab_index,
        ),
        onboarding_row(
            "onboarding-android-programming-extras-row",
            IconName::Code,
            "Programming Keys Row",
            settings.programming_extras_row,
            |content, value| content.programming_extras_row = Some(value),
            tab_index,
        ),
        onboarding_row(
            "onboarding-android-trackpad-mode",
            IconName::Crosshair,
            "Virtual Trackpad",
            settings.trackpad_mode,
            |content, value| content.trackpad_mode = Some(value),
            tab_index,
        ),
    ];
    v_flex()
        .gap_1()
        .child(Label::new("Android Input"))
        .child(
            Label::new("Touch-device input features. Toggle off any you don't want.")
                .color(Color::Muted)
                .size(LabelSize::Small),
        )
        .child(v_flex().mt_1p5().gap_2().children(rows))
        .into_any_element()
}

fn onboarding_row(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    enabled: bool,
    update: fn(&mut AndroidInputSettingsContent, bool),
    tab_index: &mut isize,
) -> AnyElement {
    let row_tab_index = *tab_index;
    *tab_index += 1;
    h_flex()
        .gap_3()
        .items_center()
        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        .child(Label::new(label))
        .child(div().flex_grow(1.))
        .child(
            Switch::new(id, ToggleState::from(enabled))
                .tab_index(row_tab_index)
                .on_click(move |&selection, _, cx| {
                    let value = match selection {
                        ToggleState::Selected => true,
                        ToggleState::Unselected => false,
                        ToggleState::Indeterminate => return,
                    };
                    settings::update_settings_file(<dyn Fs>::global(cx), cx, move |content, _| {
                        update(content.android_input.get_or_insert_default(), value);
                    });
                }),
        )
        .into_any_element()
}

pub struct SoftKeyboardButton {
    _settings_subscription: Subscription,
}

impl SoftKeyboardButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }
}

impl Render for SoftKeyboardButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One UI's Samsung Keyboard doesn't show while a physical keyboard
        // blocks it, so the button would do nothing. gpui_android redraws
        // the window when that changes.
        if !AndroidInputSettings::get_global(cx).on_screen_keyboard
            || gpui_android::physical_keyboard_blocks_ime()
        {
            return div().into_any_element();
        }
        // gpui_android redraws the window when the keyboard shows or hides.
        let keyboard_visible = gpui_android::soft_keyboard_visible();
        IconButton::new("toggle_soft_keyboard", IconName::Keyboard)
            .icon_size(IconSize::Small)
            .toggle_state(keyboard_visible)
            .on_click(|_, window, _| {
                if gpui_android::soft_keyboard_visible() {
                    window.dismiss_virtual_keyboard();
                } else {
                    window.request_virtual_keyboard();
                }
            })
            .tooltip(Tooltip::text(if keyboard_visible {
                "Hide Keyboard"
            } else {
                "Show Keyboard"
            }))
            .into_any_element()
    }
}

impl StatusItemView for SoftKeyboardButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    // Shown only while `android_input.on_screen_keyboard` is on.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

pub struct TrackpadModeButton {
    _settings_subscription: Subscription,
}

impl TrackpadModeButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }
}

impl Render for TrackpadModeButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let android_input = AndroidInputSettings::get_global(cx);
        if !android_input.trackpad_mode {
            return div().into_any_element();
        }
        let trackpad_active = android_input.trackpad_mode_active;
        IconButton::new("toggle_trackpad_mode", IconName::Crosshair)
            .icon_size(IconSize::Small)
            .toggle_state(trackpad_active)
            .on_click(|_, _, cx| {
                settings::update_settings_file(<dyn Fs>::global(cx), cx, |content, _| {
                    let android_input = content.android_input.get_or_insert_default();
                    let active = android_input.trackpad_mode_active.unwrap_or(false);
                    android_input.trackpad_mode_active = Some(!active);
                });
            })
            .tooltip(Tooltip::text(if trackpad_active {
                "Disable Trackpad Mode"
            } else {
                "Enable Trackpad Mode (virtual mouse)"
            }))
            .into_any_element()
    }
}

impl StatusItemView for TrackpadModeButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    // Shown only while `android_input.trackpad_mode` is on.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
