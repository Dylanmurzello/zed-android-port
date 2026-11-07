//! Android's title bar, below `menu_bar` in the workspace's `titlebar_item`
//! slot (see `header.rs`). It has what Zed's `title_bar` crate shows without
//! calls and sign-in: Restricted Mode, the shared storage warning, the remote
//! connection, the project name, and the update button from `app_updater`.
//! The chevron toggles the menu bar; right-click or a two-finger tap on it
//! opens Settings, Keymap, Themes, Icon Themes and Extensions.

use std::path::PathBuf;

use gpui::{
    Action, Anchor, AnyElement, App, Context, DismissEvent, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Point,
    Render, SharedString, Styled, WeakEntity, Window, anchored, deferred, div,
    prelude::FluentBuilder,
};
use log::error;
use project::trusted_worktrees::TrustedWorktrees;
use recent_projects::RemoteServerProjects;
use remote::{ConnectionState, RemoteConnectionOptions};
use theme::ActiveTheme;
use ui::{
    Button, ButtonCommon, ButtonLike, ButtonStyle, Clickable, Color, ContextMenu, Icon, IconButton,
    IconName, IconSize, IconWithIndicator, Indicator, Label, LabelCommon, LabelSize, PopoverMenu,
    SelectableButton, TintColor, Tooltip, h_flex,
};
use util::ResultExt as _;
use workspace::Workspace;
use zed_actions::OpenRemote;

use crate::app_updater::{self, AppUpdater};
use crate::menu_bar::MenuBar;
use crate::noexec_modal::NoexecMoveModal;

pub struct TitleBar {
    workspace: WeakEntity<Workspace>,
    menu_bar: WeakEntity<MenuBar>,
    /// The chevron's context menu while it's open, as `project_panel` keeps
    /// its own.
    settings_menu: Option<(Entity<ContextMenu>, Point<Pixels>, gpui::Subscription)>,
    focus_handle: FocusHandle,
}

impl TitleBar {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        menu_bar: WeakEntity<MenuBar>,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(updater) = AppUpdater::global(cx) {
            cx.observe(&updater, |_, _, cx| cx.notify()).detach();
        }
        Self {
            workspace,
            menu_bar,
            settings_menu: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn worktree_abs_path(&self, cx: &Context<Self>) -> Option<PathBuf> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        let mut visible = project.visible_worktrees(cx);
        let first = visible.next()?;
        Some(first.read(cx).abs_path().to_path_buf())
    }

    fn render_noexec_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let abs_path = self.worktree_abs_path(cx)?;
        if !gpui_android::storage::is_noexec_path(&abs_path) {
            return None;
        }
        if gpui_android::storage::is_noexec_suppressed(&abs_path) {
            return None;
        }
        let basename = abs_path.file_name()?.to_string_lossy().to_string();
        let tooltip_text = format!(
            "Project lives on shared storage (FUSE noexec) — \
                    cargo / go / make / native build tools will EACCES on run. \
                    Tap to copy into ~/projects/{basename} or suppress this warning."
        );
        let click_path = abs_path.clone();
        Some(
            Button::new("zed-android-noexec-banner", "Builds won't run · Move")
                .style(ButtonStyle::Tinted(TintColor::Warning))
                .label_size(LabelSize::Small)
                .color(Color::Warning)
                .start_icon(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .tooltip(move |_, cx| Tooltip::simple(tooltip_text.as_str(), cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let path = click_path.clone();
                    let Some(workspace) = this.workspace.upgrade() else {
                        error!("noexec-banner: workspace gone, can't open modal");
                        return;
                    };
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |_, cx| NoexecMoveModal::new(path, cx));
                    });
                }))
                .into_any_element(),
        )
    }

    fn render_restricted_mode(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().clone();
        let has_restricted = TrustedWorktrees::try_get_global(cx)
            .map(|trusted| {
                trusted
                    .read(cx)
                    .has_restricted_worktrees(&project.read(cx).worktree_store(), cx)
            })
            .unwrap_or(false);
        if !has_restricted {
            return None;
        }

        let workspace_for_click = self.workspace.clone();
        Some(
            Button::new("zed-android-restricted-mode", "Restricted Mode")
                .style(ButtonStyle::Tinted(TintColor::Warning))
                .label_size(LabelSize::Small)
                .color(Color::Warning)
                .start_icon(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .tooltip(|_, cx| {
                    Tooltip::simple("You're in Restricted Mode — tap to trust this project", cx)
                })
                .on_click(move |_, window, cx| {
                    workspace_for_click
                        .update(cx, |workspace, cx| {
                            workspace.show_worktree_trust_security_modal(true, window, cx)
                        })
                        .log_err();
                })
                .into_any_element(),
        )
    }

    /// A copy of `render_remote_project_connection` in
    /// `crates/title_bar/src/title_bar.rs`.
    fn render_remote_project_connection(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().clone();

        let options = project.read(cx).remote_connection_options(cx)?;
        let host: SharedString = options.display_name().into();

        let (nickname, tooltip_title, icon) = match options {
            RemoteConnectionOptions::Ssh(ssh_opts) => (
                ssh_opts.nickname.map(|nick| nick.into()),
                "Remote Project",
                IconName::Server,
            ),
            RemoteConnectionOptions::Wsl(_) => (None, "Remote Project", IconName::Linux),
            RemoteConnectionOptions::Docker(_) => (None, "Dev Container", IconName::Box),
        };

        let nickname = nickname.unwrap_or_else(|| host.clone());

        let (indicator_color, meta) = match project.read(cx).remote_connection_state(cx)? {
            ConnectionState::Connecting => (Color::Info, format!("Connecting to: {host}")),
            ConnectionState::Connected => (Color::Success, format!("Connected to: {host}")),
            ConnectionState::HeartbeatMissed => (
                Color::Warning,
                format!("Connection attempt to {host} missed. Retrying..."),
            ),
            ConnectionState::Reconnecting => (
                Color::Warning,
                format!("Lost connection to {host}. Reconnecting..."),
            ),
            ConnectionState::Disconnected => (Color::Error, format!("Disconnected from {host}")),
        };

        let icon_color = match project.read(cx).remote_connection_state(cx)? {
            ConnectionState::Connecting => Color::Info,
            ConnectionState::Connected => Color::Default,
            ConnectionState::HeartbeatMissed => Color::Warning,
            ConnectionState::Reconnecting => Color::Warning,
            ConnectionState::Disconnected => Color::Error,
        };

        let meta = SharedString::from(meta);
        let workspace_for_menu = self.workspace.clone();

        Some(
            PopoverMenu::new("zed-android-remote-project-menu")
                .menu(move |window, cx| {
                    let workspace_entity = workspace_for_menu.upgrade()?;
                    let fs = workspace_entity.read(cx).project().read(cx).fs().clone();
                    Some(RemoteServerProjects::popover(
                        fs,
                        workspace_for_menu.clone(),
                        Some(false),
                        window,
                        cx,
                    ))
                })
                .trigger_with_tooltip(
                    ButtonLike::new("zed-android-remote-project")
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .child(
                            h_flex()
                                .gap_2()
                                .max_w_32()
                                .child(
                                    IconWithIndicator::new(
                                        Icon::new(icon).size(IconSize::Small).color(icon_color),
                                        Some(Indicator::dot().color(indicator_color)),
                                    )
                                    .indicator_border_color(Some(
                                        cx.theme().colors().title_bar_background,
                                    ))
                                    .into_any_element(),
                                )
                                .child(Label::new(nickname).size(LabelSize::Small).truncate()),
                        ),
                    move |_window, cx| {
                        Tooltip::with_meta(
                            tooltip_title,
                            Some(&OpenRemote {
                                from_existing_connection: false,
                                create_new_window: Some(false),
                            }),
                            meta.clone(),
                            cx,
                        )
                    },
                )
                .anchor(gpui::Anchor::TopLeft)
                .into_any_element(),
        )
    }

    fn render_project_name(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        let mut visible = project.visible_worktrees(cx);
        let first = visible.next()?;
        let worktree = first.read(cx);
        let root = worktree
            .root_name()
            .display(worktree.path_style())
            .to_string();
        Some(
            Label::new(root)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        )
    }

    fn render_chevron(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let menu_bar_for_toggle = self.menu_bar.clone();
        let toggle_click = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            menu_bar_for_toggle
                .update(cx, |bar, cx| bar.toggle(cx))
                .log_err();
        };

        let secondary = cx.listener(
            |this: &mut Self, event: &MouseDownEvent, window: &mut Window, cx| {
                this.deploy_settings_menu(event.position, window, cx);
                cx.stop_propagation();
            },
        );

        div()
            .id("zed-android-titlebar-chevron-wrap")
            .on_mouse_down(MouseButton::Right, secondary)
            .child(
                IconButton::new("zed-android-titlebar-chevron-btn", IconName::ChevronDown)
                    .icon_size(IconSize::Small)
                    .style(ButtonStyle::Subtle)
                    .tooltip(|_, cx| {
                        Tooltip::simple(
                            "Tap: toggle menu bar  ·  right-click / two-finger tap: settings",
                            cx,
                        )
                    })
                    .on_click(toggle_click),
            )
            .into_any_element()
    }

    fn deploy_settings_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.action("Settings", zed_actions::OpenSettings.boxed_clone())
                .action("Keymap", Box::new(zed_actions::OpenKeymap))
                .action(
                    "Themes…",
                    zed_actions::theme_selector::Toggle::default().boxed_clone(),
                )
                .action(
                    "Icon Themes…",
                    zed_actions::icon_theme_selector::Toggle::default().boxed_clone(),
                )
                .action(
                    "Extensions",
                    zed_actions::Extensions::default().boxed_clone(),
                )
        });
        let menu_focus = menu.focus_handle(cx);
        window.focus(&menu_focus, cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.settings_menu.take();
            cx.notify();
        });
        self.settings_menu = Some((menu, position, subscription));
        cx.notify();
    }
}

impl Focusable for TitleBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TitleBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let restricted = self.render_restricted_mode(cx);
        let noexec = self.render_noexec_banner(cx);
        let remote_pill = self.render_remote_project_connection(cx);
        let project_name = self.render_project_name(cx);
        let update_button = app_updater::render_update_button(cx);
        let chevron = self.render_chevron(cx);

        h_flex()
            .w_full()
            .h_6()
            .px_2()
            .gap_2()
            .items_center()
            .bg(cx.theme().colors().title_bar_background)
            .children(restricted)
            .children(noexec)
            .children(remote_pill)
            .children(project_name)
            .child(div().flex_1())
            .children(update_button)
            .child(chevron)
            .when_some(
                self.settings_menu
                    .as_ref()
                    .map(|(menu, position, _)| (menu.clone(), *position)),
                |container, (menu, position)| {
                    container.child(
                        deferred(
                            anchored()
                                .position(position)
                                .anchor(Anchor::TopRight)
                                .child(menu),
                        )
                        .with_priority(1),
                    )
                },
            )
            .into_any_element()
    }
}
