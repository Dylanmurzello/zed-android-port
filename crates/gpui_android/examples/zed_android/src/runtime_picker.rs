use std::sync::Arc;

use gpui::{
    Action as _, AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Render,
    Size, Window, WindowBounds, WindowKind, WindowOptions, actions, prelude::*, px,
};
use platform_title_bar::PlatformTitleBar;
use release_channel::ReleaseChannel;
use theme::ActiveTheme;
use ui::{
    Button, ButtonCommon, ButtonSize, ButtonStyle, Chip, Clickable, Color, Disableable,
    FluentBuilder, Headline, HeadlineSize, Icon, IconName, IconSize, Label, LabelCommon, LabelSize,
    ParentElement, Styled, h_flex, v_flex,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{Workspace, client_side_decorations};
use zdroid_runtime::{
    HealthStatus, RuntimeId, RuntimeProvider, adapters,
    adapters::{
        bootstrap_install::{installed_release, release_repo_from_input},
        chroot::SPAWND_RELEASE_URL,
    },
    config::{DEFAULT_RELEASE_REPO, RuntimeFile},
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
        self.tx.unbounded_send(label.to_string()).log_err();
    }
    fn progress(&mut self, _done: u64, _total: u64) {}
    fn warn(&mut self, message: &str) {
        log::warn!("zdroid_runtime_picker: warn: {}", message);
        self.tx
            .unbounded_send(format!("warning: {message}"))
            .log_err();
    }
}

// The bootstrap installer preserves this file when replacing the prefix.
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
///   - Settings → "Android" → "Open picker".
///   - Onboarding basics page → "Configure runtime" button.
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

/// The runtime picked in `zd-runtime.toml`, or `None` before the user has
/// picked one, for the onboarding page's label. `active_provider`'s fallback
/// to Bootstrap isn't a pick, so this is read from the file.
#[derive(Default)]
pub struct ActiveRuntime {
    pub current: Option<RuntimeId>,
}

impl gpui::Global for ActiveRuntime {}

/// The onboarding page's runtime section (`onboarding::AndroidBasicsSections`).
pub fn render_onboarding_section(tab_index: &mut isize, cx: &mut App) -> AnyElement {
    let current_label = match cx
        .try_global::<ActiveRuntime>()
        .and_then(|runtime| runtime.current)
    {
        Some(RuntimeId::Chroot) => "Current: Kali chroot",
        Some(RuntimeId::Bootstrap) => "Current: Bootstrap",
        Some(RuntimeId::ExternalTermux) => "External Termux unavailable — choose another runtime",
        None => "Not configured yet",
    };
    let button_tab_index = *tab_index;
    *tab_index += 1;
    v_flex()
        .gap_2()
        .child(Label::new("Android Runtime"))
        .child(
            Label::new(current_label)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            Button::new("configure-android-runtime", "Configure runtime")
                .style(ButtonStyle::OutlinedGhost)
                .size(ButtonSize::Medium)
                .label_size(LabelSize::Small)
                .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::Small))
                .tab_index(button_tab_index)
                .on_click(|_, window, cx| window.dispatch_action(PickRuntime.boxed_clone(), cx)),
        )
        .into_any_element()
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
        |window, cx| cx.new(|cx| RuntimePicker::new(window, cx)),
    )
    .log_err();
}

struct AdapterEntry {
    id: RuntimeId,
    tagline: &'static str,
    health: HealthStatus,
    /// Which release is installed, for runtimes the picker installs.
    installed: Option<String>,
}

pub struct RuntimePicker {
    title_bar: Option<Entity<PlatformTitleBar>>,
    focus_handle: FocusHandle,
    entries: Vec<AdapterEntry>,
    configuration: Option<RuntimeFile>,
    error: Option<String>,
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
    /// The GitHub repository Install downloads the bootstrap from, so
    /// another one can be used without rebuilding Zdroid. There is no
    /// terminal to edit `zd-runtime.toml` with before the first install.
    repository_field: Entity<InputField>,
}

impl RuntimePicker {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title_bar = if !cfg!(target_os = "macos") {
            Some(cx.new(|cx| PlatformTitleBar::new("runtime-picker-title-bar", cx)))
        } else {
            None
        };
        let (configuration, mut error) =
            match RuntimeFile::load(std::path::Path::new(RUNTIME_TOML_PATH)) {
                Ok(configuration) => (configuration, None),
                Err(error) => (
                    None,
                    Some(format!("Could not read runtime settings: {error:#}")),
                ),
            };
        let current = configuration.as_ref().map(|file| file.runtime.kind);
        if current == Some(RuntimeId::ExternalTermux) {
            error = Some(
                "External Termux is unavailable. Select Bootstrap or Chroot to run local tools."
                    .into(),
            );
        }
        let release_repo = configuration
            .as_ref()
            .and_then(|file| file.bootstrap.as_ref())
            .map_or(DEFAULT_RELEASE_REPO, |bootstrap| {
                bootstrap.release_repo.as_str()
            })
            .to_owned();
        let repository_field = cx.new(|cx| {
            let field = InputField::new(window, cx, DEFAULT_RELEASE_REPO).label("Download from");
            field.set_text(&release_repo, window, cx);
            field
        });
        Self {
            title_bar,
            focus_handle: cx.focus_handle(),
            entries: build_entries(configuration.as_ref()),
            configuration,
            error,
            current,
            install_status: None,
            restart_required: false,
            repository_field,
        }
    }

    /// Trigger an async bootstrap install. Drops the user into a
    /// "downloading + extracting" state on the Bootstrap card while
    /// the background task downloads the zip from the repository in
    /// the "Download from" field and extracts to `$PREFIX`. Refreshes adapter health on
    /// completion so the card flips from NotInstalled → Healthy
    /// without the user having to re-open the picker.
    fn install_bootstrap(&mut self, cx: &mut Context<Self>) {
        if self.install_status.is_some() {
            return; // already in progress
        }
        let input = self.repository_field.read(cx).text(cx);
        let release_repo = if input.trim().is_empty() {
            Some(DEFAULT_RELEASE_REPO.to_owned())
        } else {
            release_repo_from_input(&input)
        };
        let error = release_repo
            .is_none()
            .then_some("Enter a GitHub repository as owner/repo or its github.com URL.");
        self.repository_field
            .update(cx, |field, cx| field.set_error(error, cx));
        let Some(release_repo) = release_repo else {
            return;
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
        let mut config = self
            .configuration
            .as_ref()
            .and_then(|file| file.bootstrap.clone())
            .unwrap_or_default();
        config.release_repo = release_repo;
        self.install_status = Some("Starting install".into());
        self.error = None;
        cx.notify();

        let install = cx.background_spawn(async move {
            let adapter = adapters::bootstrap::BootstrapAdapter::new(config)?;
            adapter.install(&mut ChannelProgressSink { tx })
        });
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(message) = rx.next().await {
                this.update(cx, |this, cx| {
                    this.install_status = Some(message);
                    cx.notify();
                })?;
            }
            let result = install.await;
            if let Err(error) = &result {
                log::error!("zdroid_runtime_picker: install failed: {error:#}");
            }
            this.update(cx, |this, cx| {
                this.install_status = None;
                this.error = result
                    .err()
                    .map(|error| format!("Install failed: {error:#}"));
                this.entries = build_entries(this.configuration.as_ref());
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn select(&mut self, id: RuntimeId, _window: &mut Window, cx: &mut Context<Self>) {
        if Some(id) == self.current {
            log::info!("zdroid_runtime_picker: {:?} already active; no-op", id);
            return;
        }

        let path = std::path::PathBuf::from(RUNTIME_TOML_PATH);
        let result = (|| {
            // An unreadable file is replaced: the picker is how users repair it.
            let mut file = RuntimeFile::load(&path)
                .log_err()
                .flatten()
                .unwrap_or_else(|| RuntimeFile::with_defaults(id));
            file.select(id)?;
            file.save(&path)?;
            anyhow::Ok(file)
        })();
        match result {
            Ok(file) => {
                self.configuration = Some(file);
                self.error = None;
                log::info!(
                    "zdroid_runtime_picker: selected {:?} -> wrote {}; restart Zdroid to apply",
                    id,
                    path.display()
                );
                self.current = Some(id);
                // The onboarding page doesn't observe this global, so
                // refresh the windows to update its label.
                cx.set_global(ActiveRuntime { current: Some(id) });
                cx.refresh_windows();
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
                self.error = Some(format!("Could not save runtime settings: {err:#}"));
                cx.notify();
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
                render_card(
                    idx,
                    entry,
                    self.current,
                    self.install_status.as_deref(),
                    &self.repository_field,
                    cx,
                )
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
            .when_some(self.error.as_ref(), |this, error| {
                this.child(Label::new(Arc::<str>::from(error.as_str())).color(Color::Error))
            })
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
    repository_field: &Entity<InputField>,
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
    let installable = id == RuntimeId::Bootstrap
        && matches!(entry.health, HealthStatus::NotInstalled { .. })
        && install_status.is_none();

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
                })
                .when_some(entry.installed.clone(), |this, installed| {
                    this.child(
                        Label::new(installed)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .when(installable, |this| {
                    this.child(repository_field.clone()).child(
                        Label::new(
                            "A GitHub repository whose releases include bootstrap-aarch64.zip. \
                             Only Zdroid's own repository is checked against a SHA-256 built \
                             into the app.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                }),
        )
        // An unhealthy selected runtime still needs its setup action.
        .child(
            if id == RuntimeId::Chroot && !matches!(entry.health, HealthStatus::Healthy) {
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
                    .disabled(!entry.health.is_healthy())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(id, window, cx);
                    }))
                    .into_any_element()
            },
        )
        .into_any_element()
}

fn build_entries(configuration: Option<&RuntimeFile>) -> Vec<AdapterEntry> {
    let chroot_health = adapters::chroot::ChrootAdapter::new(
        configuration
            .and_then(|file| file.chroot.clone())
            .unwrap_or_default(),
    )
    .map(|a| a.health_check())
    .unwrap_or_else(|err| HealthStatus::Failed {
        error: err.to_string(),
    });
    let bootstrap_config = configuration
        .and_then(|file| file.bootstrap.clone())
        .unwrap_or_default();
    let bootstrap_installed =
        installed_release(&bootstrap_config.prefix).map(|(tag, repository)| match repository {
            Some(repository) => format!("Installed: {tag} from {repository}"),
            None => format!("Installed: {tag}"),
        });
    let bootstrap_health = adapters::bootstrap::BootstrapAdapter::new(bootstrap_config)
        .map(|a| a.health_check())
        .unwrap_or_else(|err| HealthStatus::Failed {
            error: err.to_string(),
        });
    // A cleared prefix can keep its version file, which is a dotfile.
    let bootstrap_installed = bootstrap_installed
        .filter(|_| !matches!(bootstrap_health, HealthStatus::NotInstalled { .. }));

    vec![
        AdapterEntry {
            id: RuntimeId::Chroot,
            tagline: "Fastest. Routes through the persistent zd-spawnd daemon. Requires Magisk root + the zdroid-spawnd module.",
            health: chroot_health,
            installed: None,
        },
        AdapterEntry {
            id: RuntimeId::Bootstrap,
            tagline: "Self-contained Termux-flavored userland inside Zdroid's sandbox. Bare or proot-wrapped. No root, no external app.",
            health: bootstrap_health,
            installed: bootstrap_installed,
        },
    ]
}
