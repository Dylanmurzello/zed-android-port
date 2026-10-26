//! Runtime adapter picker — lets the user pick which userland Zdroid
//! routes its spawns through.
//!
//! Opens as a standalone window from the `zdroid: pick runtime` action
//! (command palette, Settings → Android Runtime, onboarding). Renders
//! one card per adapter with name, tagline and a live health snapshot;
//! `Select` writes `zd-runtime.toml` and shows a restart banner.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Render, Size,
    Window, WindowBounds, WindowKind, WindowOptions, actions, prelude::*, px,
};
use platform_title_bar::PlatformTitleBar;
use release_channel::ReleaseChannel;
use theme::ActiveTheme;
use ui::{
    Button, Chip, Clickable, Color, FluentBuilder, Headline, HeadlineSize, Icon,
    IconName, IconSize, Label, LabelCommon, LabelSize, ParentElement, Styled, h_flex, v_flex,
};
use util::ResultExt as _;
use workspace::{Workspace, client_side_decorations};
use zdroid_runtime::{
    HealthStatus, RuntimeId, RuntimeProvider,
    adapters,
    adapters::chroot::SPAWND_RELEASE_URL,
    config::{self, ChrootConfig, RuntimeFile, RuntimeState},
};

actions!(
    zdroid_runtime,
    [
        /// Open the runtime adapter picker modal.
        PickRuntime,
    ]
);

/// Register the `zdroid_runtime::PickRuntime` action. Called from
/// `lib.rs::android_main` at workspace init. Once registered, the
/// action can be triggered from anywhere via `cx.build_action(...)` +
/// `window.dispatch_action(...)`. Three current entry points:
///
///   - Command palette (`zdroid: pick runtime`).
///   - Settings → "Android Runtime" → "Open picker".
///   - Onboarding basics page → "Set up Android runtime" button.
///
/// The handler unconditionally opens the picker as a STANDALONE
/// WINDOW (`cx.open_window`), not as a workspace Modal. The window
/// path works from any caller's window context: action dispatched
/// from inside the Settings window still spawns the picker as its
/// own independent OS window (on Android, an ExtraWindowActivity).
/// The modal path required dispatching from the workspace window and
/// rendered behind any window stacked on top — bad UX.
pub fn register(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
            workspace.register_action(handle_pick_runtime);
        },
    )
    .detach();
}

fn handle_pick_runtime(
    _workspace: &mut Workspace,
    _: &PickRuntime,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_runtime_picker_window(window, cx);
}

/// Spawn the runtime picker as its own window. Public so any callsite
/// (settings page on_click, onboarding button on_click, lib.rs first-
/// launch hook if we ever add one back) can open the same picker with
/// the same parameters. Dedupes against an already-open instance so
/// repeated taps don't pile up windows.
///
/// `_window` is unused but matches the on_click signature gpui passes
/// to settings_ui's ActionLink so we don't need a wrapper closure at
/// every call site.
pub fn open_runtime_picker_window(_window: &mut Window, cx: &mut App) {
    let existing = cx
        .windows()
        .into_iter()
        .find_map(|w| w.downcast::<RuntimePicker>());

    if let Some(existing) = existing {
        existing
            .update(cx, |_, window, _| window.activate_window())
            .log_err();
        return;
    }

    let app_id = ReleaseChannel::global(cx).app_id();
    // Sized to fit all three adapter cards on a fresh open without the
    // user having to drag the window taller. The cards (with NotInstalled
    // detail lines visible) come in around ~165px each in DP; three
    // stacked plus header, title bar, gap_3 spacing, and p_6 container
    // padding lands at ~700 DP minimum content. The 800 DP height gives
    // headroom for theme variance and Samsung DeX's chrome insets.
    // Width stays generous so the right-hand action button doesn't
    // wrap into the body text on the longest tagline.
    let window_size = Size {
        width: px(640.0),
        height: px(800.0),
    };
    let window_min_size = Size {
        width: px(480.0),
        height: px(560.0),
    };

    cx.open_window(
        WindowOptions {
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Android Runtime".into()),
                appears_transparent: true,
                traffic_light_position: Some(gpui::point(px(12.0), px(12.0))),
            }),
            focus: true,
            show: true,
            is_movable: true,
            kind: WindowKind::Normal,
            window_background: cx.theme().window_background_appearance(),
            app_id: Some(app_id.to_owned()),
            window_decorations: Some(gpui::WindowDecorations::Client),
            window_bounds: Some(WindowBounds::centered(window_size, cx)),
            window_min_size: Some(window_min_size),
            ..Default::default()
        },
        |_, cx| cx.new(RuntimePicker::new),
    )
    .log_err();
}

struct AdapterEntry {
    id: RuntimeId,
    tagline: &'static str,
    health: HealthStatus,
}

pub struct RuntimePicker {
    title_bar: Option<Entity<PlatformTitleBar>>,
    focus_handle: FocusHandle,
    entries: Vec<AdapterEntry>,
    /// The currently active adapter (from disk). Marked with a
    /// "Current" badge in the UI; `Select` is a no-op if the user
    /// picks the same one.
    current: Option<RuntimeId>,
    /// True once an adapter selection has been saved to
    /// `runtime.toml` and the user needs to fully close and reopen
    /// the app for the change to take effect. Drives the inline
    /// banner at the top of the picker. We don't attempt an in-app
    /// restart (canonical Android patterns interact poorly with
    /// Background Activity Launch rules and per-OEM task lifecycle
    /// policies); the user closes via Recents and reopens from the
    /// launcher.
    restart_required: bool,
}

impl RuntimePicker {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let title_bar = if !cfg!(target_os = "macos") {
            Some(cx.new(|cx| PlatformTitleBar::new("runtime-picker-title-bar", cx)))
        } else {
            None
        };
        Self {
            title_bar,
            focus_handle: cx.focus_handle(),
            entries: build_entries(),
            current: detect_current(),
            restart_required: false,
        }
    }

    fn select(&mut self, id: RuntimeId, _window: &mut Window, cx: &mut Context<Self>) {
        if Some(id) == self.current {
            log::info!(
                "zdroid_runtime_picker: {:?} already active; no-op",
                id
            );
            return;
        }

        let path = config::runtime_toml_path(&config::data_path());
        let file = RuntimeFile::with_defaults(id);
        match file.save(&path) {
            Ok(()) => {
                log::info!(
                    "zdroid_runtime_picker: selected {:?} -> wrote {}; restart Zdroid to apply",
                    id,
                    path.display()
                );
                self.current = Some(id);
                // Update the gpui Global so any entity observing it
                // (e.g. the onboarding page's "Current: <adapter>"
                // label) re-renders without waiting for an app
                // restart. set_global pushes
                // NotifyGlobalObservers which fans out to every
                // registered observer.
                cx.set_global(onboarding::runtime_global::ActiveRuntime {
                    current: Some(id),
                });
                cx.notify();

                // Surface the close-and-reopen requirement inline,
                // styled with the picker's own theme — see Render
                // for the banner. Window-level `window.prompt` is
                // the native Android AlertDialog which looks out of
                // place against the editor's chrome. We deliberately
                // don't attempt an in-app restart either; the
                // canonical approaches (AlarmManager + PendingIntent,
                // startActivity + delayed kill, ActivityManager
                // appTasks sweep) all interact poorly with Android's
                // evolving Background Activity Launch rules and per-
                // OEM task lifecycle policies.
                self.restart_required = true;
            }
            Err(err) => {
                log::error!(
                    "zdroid_runtime_picker: failed to write {}: {:#}",
                    path.display(),
                    err
                );
            }
        }
    }
}

impl Focusable for RuntimePicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RuntimePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Copy out the few theme colors we need so the immutable borrow
        // of `cx.theme()` doesn't conflict with the mutable borrow we
        // need later for `render_card` and `client_side_decorations`.
        let bg = cx.theme().colors().editor_background;
        let text = cx.theme().colors().text;

        let cards: Vec<AnyElement> = self
            .entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                render_card(idx, entry, self.current, cx)
            })
            .collect();

        let banner = self.restart_required.then(|| {
            let border = cx.theme().colors().border;
            let banner_bg = cx.theme().colors().element_background;
            let warning = cx.theme().status().warning;
            h_flex()
                .gap_3()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(banner_bg)
                .items_start()
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Custom(warning)),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            Label::new("Restart to apply")
                                .size(LabelSize::Default)
                                .color(Color::Default),
                        )
                        .child(
                            Label::new("Runtime adapter switched.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
        });

        let content = v_flex()
            .key_context("RuntimePicker")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_6()
            .gap_4()
            .bg(bg)
            .when(cfg!(target_os = "macos"), |this| this.pt_10())
            .child(
                v_flex()
                    .gap_1()
                    .child(Headline::new("Pick your runtime").size(HeadlineSize::Medium))
                    .child(
                        Label::new(
                            "Where Zdroid runs your tools: LSPs, git, formatters, terminal. \
                             Switch any time from Settings.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .when_some(banner, |this, banner| this.child(banner))
            .child(v_flex().gap_3().children(cards));

        client_side_decorations(
            v_flex()
                .size_full()
                .text_color(text)
                .children(self.title_bar.clone())
                .child(content),
            window,
            cx,
        )
    }
}

fn render_card(
    idx: usize,
    entry: &AdapterEntry,
    current: Option<RuntimeId>,
    cx: &mut Context<RuntimePicker>,
) -> AnyElement {
    let theme_colors = cx.theme().colors();
    let id = entry.id;
    let is_current = current == Some(id);
    let name = id.display_name();
    let tagline = entry.tagline;

    let (dot_color, dot_label): (Color, &'static str) = match &entry.health {
        HealthStatus::Healthy => (Color::Success, "Ready"),
        HealthStatus::NotInstalled { .. } => (Color::Muted, "Not installed"),
        HealthStatus::Misconfigured { .. } => (Color::Warning, "Needs attention"),
        HealthStatus::Failed { .. } => (Color::Error, "Failed"),
    };

    let detail = match &entry.health {
        HealthStatus::Healthy => None,
        HealthStatus::NotInstalled { hint } => Some(hint.clone()),
        HealthStatus::Misconfigured { reason } => Some(reason.clone()),
        HealthStatus::Failed { error } => Some(error.clone()),
    };

    h_flex()
        .id(("adapter-card", idx))
        .gap_4()
        .p_4()
        .w_full()
        .border_1()
        .border_color(if is_current {
            theme_colors.border_focused
        } else {
            theme_colors.border_variant
        })
        .rounded_md()
        // Allow inner flex children to shrink below their content
        // width (CSS `min-width: 0` equivalent) — without this the
        // long tagline labels push the layout past the modal's edge.
        .min_w_0()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Icon::new(IconName::Server).size(IconSize::Small))
                        .child(Headline::new(name).size(HeadlineSize::XSmall))
                        .when(is_current, |row| {
                            // Use ui::Chip — the canonical Zed badge
                            // primitive (same one agent_ui uses for
                            // "Latest" tags etc.). Matches the rest of
                            // the editor's design language out of the
                            // box.
                            row.child(
                                Chip::new("Active")
                                    .icon(IconName::Check)
                                    .label_color(Color::Accent),
                            )
                        }),
                )
                .child(
                    Label::new(tagline)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(IconName::Circle)
                                .size(IconSize::XSmall)
                                .color(dot_color),
                        )
                        .child(
                            Label::new(dot_label)
                                .size(LabelSize::XSmall)
                                .color(dot_color),
                        ),
                )
                .when_some(detail, |this, detail| {
                    this.child(
                        Label::new(Arc::<str>::from(detail))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                }),
        )
        // Cascade priority: install actions WIN over the "Selected"
        // decorative chip, even when the unhealthy adapter is the
        // user's current runtime.toml selection; a "Selected" chip on
        // a broken adapter would leave the user stuck without a way to
        // fix it.
        .child(if id == RuntimeId::Chroot
            && !matches!(entry.health, HealthStatus::Healthy)
        {
            // Chroot adapter requires the zdroid-spawnd Magisk module
            // to be running. If the daemon socket isn't reachable,
            // letting the user pick chroot just writes a runtime.toml
            // that breaks every subsequent spawn. Surface the install
            // path inline instead: tap "Get module" to jump to the
            // GitHub releases page where the zip lives. After install
            // + reboot, re-open the picker and the gate flips to
            // Healthy → normal Select.
            Button::new(("get-module", idx), "Get module")
                .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::Small))
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.open_url(SPAWND_RELEASE_URL);
                }))
                .into_any_element()
        } else if is_current {
            // Healthy AND the active selection — decorative confirm.
            // The header already shows an "Active" Chip; this right-
            // hand Chip is design-language parity.
            Chip::new("Selected")
                .icon(IconName::Check)
                .label_color(Color::Accent)
                .into_any_element()
        } else {
            Button::new(("select", idx), "Select")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select(id, window, cx);
                }))
                .into_any_element()
        })
        .into_any_element()
}

/// Build the per-adapter health snapshot at modal-open time. Each
/// adapter is constructed with on-device defaults so the health probe
/// can run; the user's eventual `runtime.toml` overrides these when
/// the adapter is actually selected.
fn build_entries() -> Vec<AdapterEntry> {
    let chroot_health = adapters::chroot::ChrootAdapter::new(default_chroot_config())
        .map(|a| a.health_check())
        .unwrap_or_else(|err| HealthStatus::Failed {
            error: err.to_string(),
        });

    vec![AdapterEntry {
        id: RuntimeId::Chroot,
        tagline:
            "Fastest. Routes through the persistent zd-spawnd daemon. Requires Magisk root + the zdroid-spawnd module.",
        health: chroot_health,
    }]
}

fn default_chroot_config() -> ChrootConfig {
    ChrootConfig {
        root: PathBuf::from("/data/local/nhsystem/kali-arm64"),
        home_bind: PathBuf::from("/zed"),
        spawnd_socket: PathBuf::from("/data/data/com.zdroid/files/run/zd-spawn"),
        su_path: PathBuf::from("/product/bin/su"),
    }
}

/// Read the active adapter id from `zd-runtime.toml`. `None` when no
/// runtime is configured (missing, legacy, or broken file); the picker
/// just doesn't render a "Current" badge in those cases.
fn detect_current() -> Option<RuntimeId> {
    RuntimeState::load(&config::data_path()).id()
}

