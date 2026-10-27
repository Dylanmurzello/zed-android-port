//! Status bar buttons for Android input: show or hide the on-screen
//! keyboard, and switch the screen between direct touch and a virtual
//! trackpad. Each is shown only while its `android_input` setting is on.

use fs::Fs;
use gpui::{App, Context, IntoElement, Render, Subscription, Window, div};
use settings::{Settings as _, SettingsStore};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::{AndroidInputSettings, HideStatusItem, ItemHandle, StatusItemView};

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
        if !AndroidInputSettings::get_global(cx).on_screen_keyboard {
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
