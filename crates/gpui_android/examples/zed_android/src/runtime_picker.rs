//! Runtime adapter picker — lets the user pick which userland Zdroid
//! routes its spawns through (chroot, bootstrap, external Termux).
//!
//! Surfaces as a centered modal triggered by the `zdroid: pick runtime`
//! action. Mirrors Zed's welcome-page card aesthetic.
//!
//! v1 scope (this commit):
//!   - Render three cards with name, tagline, live health snapshot.
//!   - Select button per card. Click logs the choice + dismisses.
//!     Persistence to `runtime.toml` lands once we've nailed down
//!     where Zdroid stores adapter config (likely `$PREFIX/etc/zd-runtime.toml`)
//!     and added the file-write helper.
//!
//! Future scope (queued in tasks #33/#34):
//!   - Install / Uninstall buttons backed by adapter `install()` paths.
//!   - "Restart Zdroid" prompt after a switch.
//!   - First-launch auto-open when no `runtime.toml` exists yet.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Render, Size,
    Tiling, Window, WindowBounds, WindowKind, WindowOptions, actions, prelude::*, px,
};
use platform_title_bar::PlatformTitleBar;
use release_channel::ReleaseChannel;
use theme::ActiveTheme;
use ui::{
    Button, Chip, Clickable, Color, Disableable, FluentBuilder, Headline, HeadlineSize, Icon,
    IconName, IconSize, Label, LabelCommon, LabelSize, ParentElement, Styled, h_flex, v_flex,
};
use util::ResultExt as _;
use workspace::{Workspace, client_side_decorations};
use zdroid_runtime::{
    HealthStatus, RuntimeId, RuntimeProvider,
    adapters,
    adapters::chroot::SPAWND_RELEASE_URL,
    config::{BootstrapConfig, ChrootConfig, ExternalTermuxConfig, RuntimeFile},
    health::ProgressSink,
};

/// Bridges the sync `ProgressSink` trait (called from the background
/// install thread) into an async channel the foreground UI poller
/// reads. `step` and `warn` are forwarded as status strings;
/// `progress` is dropped because the install path's milestones are
/// already coarse enough to render as labels.
struct ChannelProgressSink {
    tx: futures::channel::mpsc::UnboundedSender<String>,
}

impl ProgressSink for ChannelProgressSink {
    fn step(&mut self, label: &str) {
        log::info!("zdroid_runtime_picker: step: {}", label);
        let _ = self.tx.unbounded_send(label.to_string());
    }
    fn progress(&mut self, _done: u64, _total: u64) {}
    fn warn(&mut self, message: &str) {
        log::warn!("zdroid_runtime_picker: warn: {}", message);
        let _ = self.tx.unbounded_send(format!("warning: {message}"));
    }
}

/// Where Zdroid stores the active-adapter selection. Lives inside
/// `$PREFIX/etc/` so the bootstrap-extraction step doesn't clobber it
/// (extraction doesn't touch `etc/`), and so it persists across
/// editor APK updates the same way other user state does.
const RUNTIME_TOML_PATH: &str = "/data/data/com.zdroid/files/usr/etc/zd-runtime.toml";

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
    /// Live status string while a bootstrap install is running.
    /// `None` when no install is in flight. The background task
    /// pushes updates via a channel; the foreground poller writes
    /// them here + calls `cx.notify()` so the install button's
    /// label re-renders without the user having to interact.
    install_status: Option<String>,
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
            install_status: None,
            restart_required: false,
        }
    }

    /// Trigger an async bootstrap install. Drops the user into a
    /// "downloading + extracting" state on the Bootstrap card while
    /// the background task pulls the latest release zip from GitHub
    /// and extracts to `$PREFIX`. Refreshes adapter health on
    /// completion so the card flips from NotInstalled → Healthy
    /// without the user having to re-open the picker.
    fn install_bootstrap(&mut self, cx: &mut Context<Self>) {
        if self.install_status.is_some() {
            return; // already in progress
        }
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
        self.install_status = Some("Starting install".into());
        cx.notify();

        // Background: run the actual install. Blocks on ureq +
        // zip extract. ProgressSink pushes status strings into the
        // channel; the foreground poller below picks them up.
        cx.background_executor()
            .spawn(async move {
                let config = default_bootstrap_config();
                let adapter = match adapters::bootstrap::BootstrapAdapter::new(config) {
                    Ok(a) => a,
                    Err(err) => {
                        log::error!(
                            "zdroid_runtime_picker: BootstrapAdapter::new failed: {err:#}"
                        );
                        let _ = tx.unbounded_send(format!("Failed: {err:#}"));
                        return;
                    }
                };
                let mut sink = ChannelProgressSink { tx: tx.clone() };
                if let Err(err) = adapter.install(&mut sink) {
                    log::error!(
                        "zdroid_runtime_picker: BootstrapAdapter::install failed: {err:#}"
                    );
                    let _ = tx.unbounded_send(format!("Install failed: {err:#}"));
                }
                // tx + sink drop here → channel closes → foreground exits.
            })
            .detach();

        // Foreground poll: drain the channel, write each message into
        // self.install_status + cx.notify so the card label updates.
        // When the channel closes (background task done), refresh the
        // adapter entries and clear the in-progress state.
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(msg) = rx.next().await {
                let _ = this.update(cx, |this, cx| {
                    this.install_status = Some(msg);
                    cx.notify();
                });
            }
            let _ = this.update(cx, |this, cx| {
                this.install_status = None;
                this.entries = build_entries();
                cx.notify();
            });
        })
        .detach();
    }

    fn select(&mut self, id: RuntimeId, _window: &mut Window, cx: &mut Context<Self>) {
        if Some(id) == self.current {
            log::info!(
                "zdroid_runtime_picker: {:?} already active; no-op",
                id
            );
            return;
        }

        let path = std::path::PathBuf::from(RUNTIME_TOML_PATH);
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
                render_card(idx, entry, self.current, self.install_status.as_deref(), cx)
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
    install_status: Option<&str>,
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
        // user's current runtime.toml selection. If Bootstrap is
        // selected but its $PREFIX is empty (Phase 6 fresh-install
        // state), the user needs the Install button — showing a
        // "Selected" chip there would leave them stuck without a way
        // to trigger the download.
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
        } else if id == RuntimeId::Bootstrap
            && matches!(entry.health, HealthStatus::NotInstalled { .. })
        {
            // Bootstrap adapter has its 240 MB userland in a separate
            // GitHub repo (`<release_repo>`); Phase 6 of the Termux-
            // divestment refactor stopped bundling it in the APK and
            // moved download to `BootstrapAdapter::install`. Tap
            // "Install" to kick off the async download + extract; the
            // button label switches to the live `install_status` for
            // the duration. After completion the card flips to
            // Healthy → normal Select.
            if let Some(status) = install_status {
                Button::new(("installing", idx), status.to_string())
                    .disabled(true)
                    .into_any_element()
            } else {
                Button::new(("install", idx), "Install")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.install_bootstrap(cx);
                    }))
                    .into_any_element()
            }
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
    let bootstrap_health = adapters::bootstrap::BootstrapAdapter::new(default_bootstrap_config())
        .map(|a| a.health_check())
        .unwrap_or_else(|err| HealthStatus::Failed {
            error: err.to_string(),
        });
    let termux_health =
        adapters::external_termux::ExternalTermuxAdapter::new(default_termux_config())
            .map(|a| a.health_check())
            .unwrap_or_else(|err| HealthStatus::Failed {
                error: err.to_string(),
            });

    vec![
        AdapterEntry {
            id: RuntimeId::Chroot,
            tagline:
                "Fastest. Routes through the persistent zd-spawnd daemon. Requires Magisk root + the zdroid-spawnd module.",
            health: chroot_health,
        },
        AdapterEntry {
            id: RuntimeId::Bootstrap,
            tagline:
                "Self-contained Termux-flavored userland inside Zdroid's sandbox. Bare or proot-wrapped. No root, no external app.",
            health: bootstrap_health,
        },
        AdapterEntry {
            id: RuntimeId::ExternalTermux,
            tagline:
                "Bridges to the user's installed Termux app via Intent IPC. Slowest path; uses the user's existing setup.",
            health: termux_health,
        },
    ]
}

fn default_chroot_config() -> ChrootConfig {
    ChrootConfig {
        root: PathBuf::from("/data/local/nhsystem/kali-arm64"),
        home_bind: PathBuf::from("/zed"),
        spawnd_socket: PathBuf::from("/data/data/com.zdroid/files/run/zd-spawn"),
        su_path: PathBuf::from("/product/bin/su"),
    }
}

fn default_bootstrap_config() -> BootstrapConfig {
    BootstrapConfig {
        prefix: PathBuf::from("/data/data/com.zdroid/files/usr"),
        proot_rootfs: None,
        release_repo: "Dylanmurzello/zdroid-bootstrap".into(),
    }
}

fn default_termux_config() -> ExternalTermuxConfig {
    ExternalTermuxConfig {
        package: "com.termux".into(),
        prefix: PathBuf::from("/data/data/com.termux/files/usr"),
    }
}

/// Read the active adapter id from `runtime.toml`. Returns `None` if
/// the file is missing (first-launch state) or unparseable; the picker
/// just doesn't render a "Current" badge in those cases.
fn detect_current() -> Option<RuntimeId> {
    RuntimeFile::load(std::path::Path::new(RUNTIME_TOML_PATH))
        .ok()
        .flatten()
        .map(|file| file.runtime.kind)
}

