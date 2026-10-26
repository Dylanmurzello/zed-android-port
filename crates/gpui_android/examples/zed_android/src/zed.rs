//! The parts of Zed's `crates/zed/src/zed.rs` that the app uses. The `zed`
//! crate is a binary the app can't link, so these are copies: they keep
//! upstream's names and bodies, so that a Zed sync can diff them against the
//! new release. Where a copy differs from upstream, a comment says why.

mod migrate;

use agent_settings::{UserAgentsMdState, init_user_agents_md};
use agent_ui::AgentDiffToolbar;
use anyhow::Context as _;
use breadcrumbs::Breadcrumbs;
use client::zed_urls;
use editor::{Editor, MultiBuffer};
use extension_host::ExtensionStore;
use fs::Fs;
use futures::FutureExt as _;
use futures::{StreamExt, channel::mpsc, select_biased};
use git_ui::branch_diff::BranchDiffToolbar;
use git_ui::commit_view::CommitViewToolbar;
use git_ui::git_panel::GitPanel;
use git_ui::project_diff::ProjectDiffToolbar;
use git_ui::solo_diff_view::{SoloDiffGitToolbar, SoloDiffStyleToolbar};
use git_ui::staged_diff::StagedDiffToolbar;
use git_ui::unstaged_diff::UnstagedDiffToolbar;
use git_ui_core::file_diff_view::FileDiffStyleToolbar;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, DismissEvent, Element, Entity,
    Focusable, KeyBinding, ParentElement, PathPromptOptions, PromptLevel, ReadGlobal, SharedString,
    Task, TaskExt, UpdateGlobal, WeakEntity, Window, WindowHandle, actions, div, image_cache, px,
    retain_all,
};
use image_viewer::ImageInfo;
use language::Capability;
use language_tools::lsp_button::{self, LspButton};
use language_tools::lsp_log_view::LspLogToolbarItemView;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use migrate::{MigrationBanner, MigrationEvent, MigrationNotification, MigrationType};
use migrator::migrate_keymap;
use onboarding::multibuffer_hint::MultibufferHint;
use outline_panel::OutlinePanel;
use paths::{
    local_debug_file_relative_path, local_settings_file_relative_path,
    local_tasks_file_relative_path,
};
use project::{
    DirectoryLister, DisableAiSettings, ProjectItem,
    project_settings::{SettingsObserver, SettingsObserverEvent},
};
use project_panel::ProjectPanel;
use recent_projects::open_remote_project;
use rope::Rope;
use search::project_search::ProjectSearchBar;
use settings::{
    BaseKeymap, DEFAULT_KEYMAP_PATH, InvalidSettingsError, KeybindSource, KeymapFile,
    KeymapFileLoadResult, MigrationStatus, SPECIFIC_OVERRIDES_KEYMAP_PATH, Settings, SettingsFile,
    SettingsStore, VIM_KEYMAP_PATH, initial_local_debug_tasks_content,
    initial_project_settings_content, initial_tasks_content, update_settings_file,
};
use sidebar::Sidebar;
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{self, AtomicBool},
};
use terminal_view::terminal_panel::{self, TerminalPanel};
use theme::{SystemAppearance, ThemeRegistry};
use theme_settings::{ThemeSettings, load_user_theme};
use ui::{IconName, PopoverMenuHandle, StyledTypography as _};
use util::ResultExt;
use util::markdown::MarkdownString;
use util::rel_path::RelPath;
use vim_mode_setting::VimModeSetting;
use workspace::notifications::{NotificationId, dismiss_app_notification, show_app_notification};
use workspace::{
    AppState, CloseWindow, MultiWorkspace, Pane, Panel, Workspace, WorkspaceSettings,
    create_and_open_local_file, notifications::simple_message_notification::MessageNotification,
    with_active_or_new_workspace,
};
use zed_actions::{
    GetMerch, OpenAccountSettings, OpenBrowser, OpenDocs, OpenProjectTasks, OpenServerSettings,
    OpenSettingsFile, OpenStatusPage, Quit,
};

const DOCS_URL: &str = "https://zed.dev/docs/";
const STATUS_URL: &str = "https://status.zed.dev";
const MERCH_URL: &str = "https://merch.zed.dev/";

actions!(
    zed,
    [
        /// Opens the default settings file.
        OpenDefaultSettings,
        /// Opens project-specific settings file.
        OpenProjectSettingsFile,
        /// Opens the tasks panel.
        OpenTasks,
        /// Opens debug tasks configuration.
        OpenDebugTasks,
        /// Shows the default semantic token rules (read-only).
        ShowDefaultSemanticTokenRules,
    ]
);

// Left out: hiding the app (macOS), the title bar's banner, test panics, the
// log actions (`logging.rs` registers the app's own), the licenses file, which
// only Zed's release bundles generate, and the About window.
pub fn init(cx: &mut App) {
    cx.on_action(quit);

    cx.on_action(|&zed_actions::OpenKeymapFile, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::keymap_file(),
                || settings::initial_keymap_content().as_ref().into(),
                window,
                cx,
            );
        });
    })
    .on_action(|_: &OpenSettingsFile, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::settings_file(),
                || settings::initial_user_settings_content().as_ref().into(),
                window,
                cx,
            );
        });
    })
    .on_action(|_: &OpenAccountSettings, cx| {
        with_active_or_new_workspace(cx, |_, _, cx| {
            cx.open_url(&zed_urls::account_url(cx));
        });
    })
    .on_action(|_: &OpenTasks, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::tasks_file(),
                || settings::initial_tasks_content().as_ref().into(),
                window,
                cx,
            );
        });
    })
    .on_action(|_: &OpenDebugTasks, cx| {
        with_active_or_new_workspace(cx, |_, window, cx| {
            open_settings_file(
                paths::debug_scenarios_file(),
                || settings::initial_debug_tasks_content().as_ref().into(),
                window,
                cx,
            );
        });
    })
    .on_action(|_: &ShowDefaultSemanticTokenRules, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_bundled_file(
                workspace,
                settings::default_semantic_token_rules(),
                "Default Semantic Token Rules",
                "JSONC",
                window,
                cx,
            );
        });
    })
    .on_action(|_: &OpenDefaultSettings, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_bundled_file(
                workspace,
                settings::default_settings(),
                "Default Settings",
                "JSON",
                window,
                cx,
            );
        });
    })
    .on_action(|_: &zed_actions::OpenDefaultKeymap, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_bundled_file(
                workspace,
                settings::default_keymap(),
                "Default Key Bindings",
                "JSON",
                window,
                cx,
            );
        });
    });
}

// Left out: quitting when the last window closes, which Android's activity
// lifecycle owns, the window appearance (Android ignores it), telemetry, the
// file watcher and GPU warnings (Linux and Windows only), and the edit
// prediction button: the app registers no AI model providers.
pub fn initialize_workspace(app_state: Arc<AppState>, cx: &mut App) {
    init_cursor_hide_mode(cx);
    init_reduce_motion(cx);
    init_global_config_error_notifications(cx);

    cx.observe_new(|_multi_workspace: &mut MultiWorkspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let multi_workspace_handle = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            multi_workspace_handle
                .update(cx, |multi_workspace, cx| {
                    // We'll handle closing asynchronously
                    multi_workspace.close_window(&CloseWindow, window, cx);
                    false
                })
                .unwrap_or(true)
        });

        let window_handle = window.window_handle();
        let multi_workspace_handle = cx.entity();
        cx.subscribe_in(
            &multi_workspace_handle,
            window,
            |this, _multi_workspace, event: &workspace::MultiWorkspaceEvent, window, cx| {
                let workspace::MultiWorkspaceEvent::ActiveWorkspaceChanged { source_workspace } =
                    event
                else {
                    return;
                };

                let active_workspace = this.workspace().clone();
                let source_workspace = source_workspace.clone();
                active_workspace.update(cx, |workspace, cx| {
                    if let Some(ref source) = source_workspace {
                        if let Some(panel) = workspace.panel::<agent_ui::AgentPanel>(cx) {
                            panel.update(cx, |panel, cx| {
                                panel.initialize_from_source_workspace_if_needed(
                                    source.clone(),
                                    window,
                                    cx,
                                );
                            });
                        }
                    }

                    ensure_agent_panel_for_workspace(workspace, source_workspace, window, cx)
                        .detach_and_log_err(cx);
                });
            },
        )
        .detach();

        cx.defer(move |cx| {
            window_handle
                .update(cx, |_, window, cx| {
                    let sidebar =
                        cx.new(|cx| Sidebar::new(multi_workspace_handle.clone(), window, cx));
                    multi_workspace_handle.update(cx, |multi_workspace, cx| {
                        multi_workspace.register_sidebar(sidebar, cx);
                    });
                })
                .ok();
        });
    })
    .detach();

    cx.observe_new(move |workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let workspace_handle = cx.entity();
        let center_pane = workspace.active_pane().clone();
        initialize_pane(workspace, &center_pane, window, cx);

        cx.subscribe_in(&workspace_handle, window, {
            move |workspace, _, event, window, cx| match event {
                workspace::Event::PaneAdded(pane) => {
                    initialize_pane(workspace, pane, window, cx);
                }
                workspace::Event::OpenBundledFile {
                    text,
                    title,
                    language,
                } => open_bundled_file(workspace, text.clone(), title, language, window, cx),
                _ => {}
            }
        })
        .detach();

        let search_button = cx.new(|_| search::search_status_button::SearchButton::new());
        let diagnostic_summary =
            cx.new(|cx| diagnostics::items::DiagnosticIndicator::new(workspace, cx));
        let active_file_name = cx.new(|_| workspace::active_file_name::ActiveFileName::new());
        let activity_indicator = activity_indicator::ActivityIndicator::new(workspace, window, cx);
        let active_buffer_encoding =
            cx.new(|_| encoding_selector::ActiveBufferEncoding::new(workspace));
        let active_buffer_language =
            cx.new(|_| language_selector::ActiveBufferLanguage::new(workspace));
        let active_toolchain_language =
            cx.new(|cx| toolchain_selector::ActiveToolchain::new(workspace, window, cx));
        let vim_mode_indicator = cx.new(|cx| vim::ModeIndicator::new(window, cx));
        let pending_keystrokes_indicator =
            cx.new(|cx| which_key::PendingKeystrokesIndicator::new(window, cx));
        let image_info = cx.new(|_cx| ImageInfo::new(workspace));

        let lsp_button_menu_handle = PopoverMenuHandle::default();
        let lsp_button =
            cx.new(|cx| LspButton::new(workspace, lsp_button_menu_handle.clone(), window, cx));
        workspace.register_action({
            move |_, _: &lsp_button::ToggleMenu, window, cx| {
                lsp_button_menu_handle.toggle(window, cx);
            }
        });

        let cursor_position =
            cx.new(|_| go_to_line::cursor_position::CursorPosition::new(workspace));
        let line_ending_indicator =
            cx.new(|_| line_ending_selector::LineEndingIndicator::default());
        let git_blame_status = cx.new(|_| git_ui::GitBlameStatus::default());
        let merge_conflict_indicator =
            cx.new(|cx| git_ui::MergeConflictIndicator::new(workspace, cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_left_item(search_button, window, cx);
            status_bar.add_left_item(lsp_button, window, cx);
            status_bar.add_left_item(diagnostic_summary, window, cx);
            status_bar.add_left_item(active_file_name, window, cx);
            status_bar.add_left_item(git_blame_status, window, cx);
            status_bar.add_left_item(merge_conflict_indicator, window, cx);
            status_bar.add_left_item(activity_indicator, window, cx);
            status_bar.add_right_item(active_buffer_encoding, window, cx);
            status_bar.add_right_item(active_buffer_language, window, cx);
            status_bar.add_right_item(active_toolchain_language, window, cx);
            status_bar.add_right_item(line_ending_indicator, window, cx);
            status_bar.add_right_item(cursor_position, window, cx);
            status_bar.add_right_item(image_info, window, cx);
            // Keep these last so they stay leftmost and can change without moving the other items.
            status_bar.add_right_item(vim_mode_indicator, window, cx);
            status_bar.add_right_item(pending_keystrokes_indicator, window, cx);
        });

        let panels_task = initialize_panels(window, cx);
        workspace.set_panels_task(panels_task);
        register_actions(app_state.clone(), workspace, window, cx);

        if !workspace.has_active_modal(window, cx) {
            workspace.focus_handle(cx).focus(window, cx);
        }
    })
    .detach();
}

// Left out: the collab and debug panels, whose crates the app doesn't build.
fn initialize_panels(window: &mut Window, cx: &mut Context<Workspace>) -> Task<anyhow::Result<()>> {
    cx.spawn_in(window, async move |workspace_handle, cx| {
        let project_panel = ProjectPanel::load(workspace_handle.clone(), cx.clone());
        let outline_panel = OutlinePanel::load(workspace_handle.clone(), cx.clone());
        let terminal_panel = TerminalPanel::load(workspace_handle.clone(), cx.clone());
        let git_panel = GitPanel::load(workspace_handle.clone(), cx.clone());

        async fn add_panel_when_ready(
            panel_task: impl Future<Output = anyhow::Result<Entity<impl workspace::Panel>>> + 'static,
            workspace_handle: WeakEntity<Workspace>,
            mut cx: gpui::AsyncWindowContext,
        ) {
            if let Some(panel) = panel_task.await.context("failed to load panel").log_err()
            {
                workspace_handle
                    .update_in(&mut cx, |workspace, window, cx| {
                        workspace.add_panel(panel, window, cx);
                    })
                    .log_err();
            }
        }

        futures::join!(
            add_panel_when_ready(project_panel, workspace_handle.clone(), cx.clone()),
            add_panel_when_ready(outline_panel, workspace_handle.clone(), cx.clone()),
            add_panel_when_ready(terminal_panel, workspace_handle.clone(), cx.clone()),
            add_panel_when_ready(git_panel, workspace_handle.clone(), cx.clone()),
            initialize_agent_panel(workspace_handle.clone(), cx.clone()).map(|r| r.log_err()),
        );

        workspace_handle.update(cx, |workspace, cx| {
            workspace.finish_dock_restoration(cx);
        })?;

        anyhow::Ok(())
    })
}

fn setup_or_teardown_ai_panel<P: Panel>(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    load_panel: impl FnOnce(
        WeakEntity<Workspace>,
        AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<P>>>
    + 'static,
) -> Task<anyhow::Result<()>> {
    let disable_ai = SettingsStore::global(cx)
        .get::<DisableAiSettings>(None)
        .disable_ai
        || cfg!(test);
    let existing_panel = workspace.panel::<P>(cx);
    match (disable_ai, existing_panel) {
        (false, None) => cx.spawn_in(window, async move |workspace, cx| {
            let panel = load_panel(workspace.clone(), cx.clone()).await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let disable_ai = SettingsStore::global(cx)
                    .get::<DisableAiSettings>(None)
                    .disable_ai;
                let have_panel = workspace.panel::<P>(cx).is_some();
                if !disable_ai && !have_panel {
                    workspace.add_panel(panel, window, cx);
                }
            })
        }),
        (true, Some(existing_panel)) => {
            workspace.remove_panel::<P>(&existing_panel, window, cx);
            Task::ready(Ok(()))
        }
        _ => Task::ready(Ok(())),
    }
}
fn ensure_agent_panel_for_workspace(
    workspace: &mut Workspace,
    source_workspace: Option<WeakEntity<Workspace>>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let task = setup_or_teardown_ai_panel(workspace, window, cx, move |workspace, cx| {
        agent_ui::AgentPanel::load(workspace, cx)
    });

    cx.spawn_in(window, async move |workspace, cx| {
        task.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            if let Some(source_workspace) = source_workspace.clone()
                && let Some(panel) = workspace.panel::<agent_ui::AgentPanel>(cx)
            {
                panel.update(cx, |panel, cx| {
                    panel.initialize_from_source_workspace_if_needed(source_workspace, window, cx);
                });
            }
        })
    })
}
async fn initialize_agent_panel(
    workspace_handle: WeakEntity<Workspace>,
    mut cx: AsyncWindowContext,
) -> anyhow::Result<()> {
    workspace_handle
        .update_in(&mut cx, |workspace, window, cx| {
            ensure_agent_panel_for_workspace(workspace, None, window, cx)
        })?
        .await?;

    workspace_handle.update_in(&mut cx, |workspace, window, cx| {
        cx.observe_global_in::<SettingsStore>(window, move |workspace, window, cx| {
            ensure_agent_panel_for_workspace(workspace, None, window, cx).detach_and_log_err(cx);
        })
        .detach();

        // Register the actions that are shared between `assistant` and `assistant2`.
        //
        // We need to do this here instead of within the individual `init`
        // functions so that we only register the actions once.
        //
        // Once we ship `assistant2` we can push this back down into `agent::agent_panel::init`.
        if !cfg!(test) {
            workspace
                .register_action(agent_ui::AgentPanel::toggle_focus)
                .register_action(agent_ui::AgentPanel::focus)
                .register_action(agent_ui::AgentPanel::toggle)
                .register_action(agent_ui::InlineAssistant::inline_assist);
        }
    })?;

    anyhow::Ok(())
}

// Left out: actions for crates the app doesn't build (input latency,
// accessibility, collab, the URL prompt, the CLI and zed:// links), window
// actions Android ignores (minimize, zoom, full screen), the frame overlay,
// which needs gpui's `profiler` feature (it records every foreground event, so
// the app leaves it off), and `workspace::Open`, `NewWindow`, `NewFile` and
// `CloseProject`, which the app handles itself.
fn register_actions(
    app_state: Arc<AppState>,
    workspace: &mut Workspace,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace
        .register_action(|_, _: &OpenDocs, _, cx| cx.open_url(DOCS_URL))
        .register_action(|_, _: &OpenStatusPage, _, cx| cx.open_url(STATUS_URL))
        .register_action(|_, _: &GetMerch, _, cx| cx.open_url(MERCH_URL))
        .register_action(|workspace, action: &OpenBrowser, _window, cx| {
            // Parse and validate the URL to ensure it's properly formatted
            match url::Url::parse(&action.url) {
                Ok(parsed_url) => {
                    // Use the parsed URL's string representation which is properly escaped
                    cx.open_url(parsed_url.as_str());
                }
                Err(e) => {
                    workspace.show_error(
                        format!(
                            "Opening this URL in a browser failed because the URL is invalid: {}\n\nError was: {e}",
                            action.url
                        ),
                        cx,
                    );
                }
            }
        })
        .register_action(|workspace, action: &zed_actions::OpenRemote, window, cx| {
            if !action.from_existing_connection {
                cx.propagate();
                return;
            }
            // You need existing remote connection to open it this way
            if workspace.project().read(cx).is_local() {
                return;
            }
            let create_new_window = action.create_new_window.unwrap_or_else(|| {
                matches!(
                    WorkspaceSettings::get_global(cx).default_open_behavior,
                    settings::DefaultOpenBehavior::NewWindow
                )
            });
            telemetry::event!("Project Opened");
            let paths = workspace.prompt_for_open_path(
                PathPromptOptions {
                    files: true,
                    directories: true,
                    multiple: true,
                    prompt: None,
                },
                DirectoryLister::Project(workspace.project().clone()),
                window,
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let Some(paths) = paths.await.log_err().flatten() else {
                    return;
                };
                if let Some(task) = this
                    .update_in(cx, |this, window, cx| {
                        open_new_ssh_project_from_project(
                            this,
                            paths,
                            create_new_window,
                            window,
                            cx,
                        )
                    })
                    .log_err()
                {
                    task.await.log_err();
                }
            })
            .detach()
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::IncreaseUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, cx| {
                        let ui_font_size = ThemeSettings::get_global(cx).ui_font_size(cx) + px(1.0);
                        let _ = settings
                            .theme
                            .ui_font_size
                            .insert(f32::from(theme_settings::clamp_font_size(ui_font_size)).into());
                    });
                } else {
                    theme_settings::adjust_ui_font_size(cx, |size| size + px(1.0));
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::DecreaseUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, cx| {
                        let ui_font_size = ThemeSettings::get_global(cx).ui_font_size(cx) - px(1.0);
                        let _ = settings
                            .theme
                            .ui_font_size
                            .insert(f32::from(theme_settings::clamp_font_size(ui_font_size)).into());
                    });
                } else {
                    theme_settings::adjust_ui_font_size(cx, |size| size - px(1.0));
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::ResetUiFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, _| {
                        settings.theme.ui_font_size = None;
                    });
                } else {
                    theme_settings::reset_ui_font_size(cx);
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::IncreaseBufferFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, cx| {
                        let buffer_font_size =
                            ThemeSettings::get_global(cx).buffer_font_size(cx) + px(1.0);
                        let _ = settings
                            .theme
                            .buffer_font_size
                            .insert(f32::from(theme_settings::clamp_font_size(buffer_font_size)).into());
                    });
                } else {
                    theme_settings::increase_buffer_font_size(cx);
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::DecreaseBufferFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, cx| {
                        let buffer_font_size =
                            ThemeSettings::get_global(cx).buffer_font_size(cx) - px(1.0);
                        let _ = settings
                            .theme
                            .buffer_font_size
                            .insert(f32::from(theme_settings::clamp_font_size(buffer_font_size)).into());
                    });
                } else {
                    theme_settings::decrease_buffer_font_size(cx);
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::ResetBufferFontSize, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, _| {
                        settings.theme.buffer_font_size = None;
                    });
                } else {
                    theme_settings::reset_buffer_font_size(cx);
                }
            }
        })
        .register_action({
            let fs = app_state.fs.clone();
            move |_, action: &zed_actions::ResetAllZoom, _window, cx| {
                if action.persist {
                    update_settings_file(fs.clone(), cx, move |settings, _| {
                        settings.theme.ui_font_size = None;
                        settings.theme.buffer_font_size = None;
                        settings.theme.agent_ui_font_size = None;
                        settings.theme.agent_buffer_font_size = None;
                    });
                } else {
                    theme_settings::reset_ui_font_size(cx);
                    theme_settings::reset_buffer_font_size(cx);
                    theme_settings::reset_agent_ui_font_size(cx);
                    theme_settings::reset_agent_buffer_font_size(cx);
                }
            }
        })
        .register_action(open_project_settings_file)
        .register_action(open_project_tasks_file)
        .register_action(open_worktree_setup_tasks_file)
        .register_action(open_project_debug_tasks_file)
        .register_action(
            |workspace: &mut Workspace,
             _: &zed_actions::project_panel::ToggleFocus,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                workspace.toggle_panel_focus::<ProjectPanel>(window, cx);
            },
        )
        .register_action(
            |workspace: &mut Workspace,
             _: &outline_panel::ToggleFocus,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                workspace.toggle_panel_focus::<OutlinePanel>(window, cx);
            },
        )
        .register_action(
            |workspace: &mut Workspace,
             _: &terminal_panel::ToggleFocus,
             window: &mut Window,
             cx: &mut Context<Workspace>| {
                workspace.toggle_panel_focus::<TerminalPanel>(window, cx);
            },
        );

    if workspace.project().read(cx).is_via_remote_server() {
        workspace.register_action({
            move |workspace, _: &OpenServerSettings, window, cx| {
                let open_server_settings = workspace
                    .project()
                    .update(cx, |project, cx| project.open_server_settings(cx));

                cx.spawn_in(window, async move |workspace, cx| {
                    let buffer = open_server_settings.await?;

                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(
                                buffer
                                    .read(cx)
                                    .project_path(cx)
                                    .expect("Settings file must have a location"),
                                None,
                                true,
                                window,
                                cx,
                            )
                        })?
                        .await?;

                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
        });
    }

    workspace.register_action(sidebar::dump_workspace_info);
}

// Left out: the toolbar items of crates the app doesn't build (the quick
// action bar, the debugger and telemetry logs, and the basedpyright banner).
fn initialize_pane(
    workspace: &Workspace,
    pane: &Entity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = cx.weak_entity();
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let multibuffer_hint = cx.new(|_| MultibufferHint::new());
            toolbar.add_item(multibuffer_hint, window, cx);
            let solo_diff_style_toolbar = cx.new(SoloDiffStyleToolbar::new);
            toolbar.add_item(solo_diff_style_toolbar, window, cx);
            let file_diff_style_toolbar = cx.new(FileDiffStyleToolbar::new);
            toolbar.add_item(file_diff_style_toolbar, window, cx);
            let breadcrumbs = cx.new(|_| Breadcrumbs::new());
            toolbar.add_item(breadcrumbs, window, cx);
            let buffer_search_bar = cx.new(|cx| {
                search::BufferSearchBar::new(
                    Some(workspace.project().read(cx).languages().clone()),
                    window,
                    cx,
                )
            });
            toolbar.add_item(buffer_search_bar.clone(), window, cx);
            let diagnostic_editor_controls = cx.new(|_| diagnostics::ToolbarControls::new());
            toolbar.add_item(diagnostic_editor_controls, window, cx);
            let project_search_bar = cx.new(|_| ProjectSearchBar::new());
            toolbar.add_item(project_search_bar, window, cx);
            let lsp_log_item = cx.new(|_| LspLogToolbarItemView::new());
            toolbar.add_item(lsp_log_item, window, cx);
            let acp_tools_item = cx.new(|_| acp_tools::AcpToolsToolbarItemView::new());
            toolbar.add_item(acp_tools_item, window, cx);
            let syntax_tree_item = cx.new(|_| language_tools::SyntaxTreeToolbarItemView::new());
            toolbar.add_item(syntax_tree_item, window, cx);
            let migration_banner =
                cx.new(|inner_cx| MigrationBanner::new(workspace_handle.clone(), inner_cx));
            toolbar.add_item(migration_banner, window, cx);
            let highlights_tree_item =
                cx.new(|_| language_tools::HighlightsTreeToolbarItemView::new());
            toolbar.add_item(highlights_tree_item, window, cx);
            let project_diff_toolbar = cx.new(|cx| ProjectDiffToolbar::new(workspace, cx));
            toolbar.add_item(project_diff_toolbar, window, cx);
            let staged_diff_toolbar = cx.new(|cx| StagedDiffToolbar::new(workspace, cx));
            toolbar.add_item(staged_diff_toolbar, window, cx);
            let unstaged_diff_toolbar = cx.new(|cx| UnstagedDiffToolbar::new(workspace, cx));
            toolbar.add_item(unstaged_diff_toolbar, window, cx);
            let branch_diff_toolbar = cx.new(BranchDiffToolbar::new);
            toolbar.add_item(branch_diff_toolbar, window, cx);
            let solo_diff_git_toolbar = cx.new(SoloDiffGitToolbar::new);
            toolbar.add_item(solo_diff_git_toolbar, window, cx);
            let commit_view_toolbar = cx.new(|_| CommitViewToolbar::new());
            toolbar.add_item(commit_view_toolbar, window, cx);
            let agent_diff_toolbar = cx.new(AgentDiffToolbar::new);
            toolbar.add_item(agent_diff_toolbar, window, cx);
            let image_view_toolbar = cx.new(|_| image_viewer::ImageViewToolbarControls::new());
            toolbar.add_item(image_view_toolbar, window, cx);
        })
    });
}

static WAITING_QUIT_CONFIRMATION: AtomicBool = AtomicBool::new(false);
fn quit(_: &Quit, cx: &mut App) {
    if WAITING_QUIT_CONFIRMATION.load(atomic::Ordering::Acquire) {
        return;
    }

    let should_confirm = WorkspaceSettings::get_global(cx).confirm_quit;
    cx.spawn(async move |cx| {
        let mut workspace_windows: Vec<WindowHandle<MultiWorkspace>> = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<MultiWorkspace>())
                .collect::<Vec<_>>()
        });

        // If multiple windows have unsaved changes, and need a save prompt,
        // prompt in the active window before switching to a different window.
        cx.update(|cx| {
            workspace_windows.sort_by_key(|window| window.is_active(cx) == Some(false));
        });

        if should_confirm && let Some(multi_workspace) = workspace_windows.first() {
            let answer = multi_workspace
                .update(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Info,
                        "Are you sure you want to quit?",
                        None,
                        &["Quit", "Cancel"],
                        cx,
                    )
                })
                .log_err();

            if let Some(answer) = answer {
                WAITING_QUIT_CONFIRMATION.store(true, atomic::Ordering::Release);
                let answer = answer.await.ok();
                WAITING_QUIT_CONFIRMATION.store(false, atomic::Ordering::Release);
                if answer != Some(0) {
                    return Ok(());
                }
            }
        }

        if !workspace::prepare_windows_to_quit(&workspace_windows, cx).await {
            return Ok(());
        }

        cx.update(|cx| cx.quit());
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn notify_settings_errors(result: settings::SettingsParseResult, is_user: bool, cx: &mut App) {
    if let settings::ParseStatus::Failed { error: err } = &result.parse_status {
        let settings_type = if is_user { "user" } else { "global" };
        log::error!("Failed to load {} settings: {err}", settings_type);
    }

    let error = match result.parse_status {
        settings::ParseStatus::Failed { error } => Some(anyhow::format_err!(error)),
        settings::ParseStatus::Success => None,
        settings::ParseStatus::Unchanged => return,
    };
    let id = NotificationId::Named(format!("failed-to-parse-settings-{is_user}").into());

    let showed_parse_error = match error {
        Some(error) => {
            if let Some(InvalidSettingsError::LocalSettings { .. }) =
                error.downcast_ref::<InvalidSettingsError>()
            {
                false
                // Local settings errors are displayed by the projects
            } else {
                show_app_notification(id, cx, move |cx| {
                    cx.new(|cx| {
                        MessageNotification::new(format!("Invalid user settings file\n{error}"), cx)
                            .primary_message("Open Settings File")
                            .primary_icon(IconName::Settings)
                            .primary_on_click(|window, cx| {
                                window.dispatch_action(
                                    zed_actions::OpenSettingsFile.boxed_clone(),
                                    cx,
                                );
                                cx.emit(DismissEvent);
                            })
                    })
                });
                true
            }
        }
        None => {
            dismiss_app_notification(&id, cx);
            false
        }
    };
    let id = NotificationId::Named(format!("failed-to-migrate-settings-{is_user}").into());

    match result.migration_status {
        settings::MigrationStatus::Succeeded | settings::MigrationStatus::NotNeeded => {
            dismiss_app_notification(&id, cx);
        }
        settings::MigrationStatus::Failed { error: err } => {
            if !showed_parse_error {
                show_app_notification(id, cx, move |cx| {
                    cx.new(|cx| {
                        MessageNotification::new(
                            format!(
                                "Failed to migrate settings\n\
                                {err}"
                            ),
                            cx,
                        )
                        .primary_message("Open Settings File")
                        .primary_icon(IconName::Settings)
                        .primary_on_click(|window, cx| {
                            window.dispatch_action(zed_actions::OpenSettingsFile.boxed_clone(), cx);
                            cx.emit(DismissEvent);
                        })
                    })
                });
            }
        }
    };
}

fn init_global_config_error_notifications(cx: &mut App) {
    cx.observe_new(|_: &mut SettingsObserver, _, cx| {
        cx.subscribe_self::<SettingsObserverEvent>(|_, event, cx| {
            let (result, file_kind, on_click): (_, _, fn(&mut Window, &mut App)) = match event {
                SettingsObserverEvent::GlobalTasksUpdated(result) => {
                    (result, "tasks", |window, cx| {
                        window.dispatch_action(OpenTasks.boxed_clone(), cx)
                    })
                }
                SettingsObserverEvent::GlobalDebugScenariosUpdated(result) => {
                    (result, "debug scenarios", |window, cx| {
                        window.dispatch_action(OpenDebugTasks.boxed_clone(), cx)
                    })
                }
                _ => return,
            };
            let id = NotificationId::Named(format!("invalid-global-{file_kind}-file").into());
            match result {
                Ok(_) => dismiss_app_notification(&id, cx),
                Err(error) => {
                    let message = format!("Invalid global {file_kind} file\n{error}");
                    show_app_notification(id, cx, move |cx| {
                        cx.new(|cx| {
                            MessageNotification::new(message.clone(), cx)
                                .primary_message("Open File")
                                .primary_icon(IconName::Settings)
                                .primary_on_click(move |window, cx| {
                                    on_click(window, cx);
                                    cx.emit(DismissEvent);
                                })
                        })
                    });
                }
            }
        })
        .detach();
    })
    .detach();
}

#[derive(Copy, Clone, Debug, settings::RegisterSetting)]
struct CursorHideModeSetting(gpui::CursorHideMode);

impl Settings for CursorHideModeSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(match content.hide_mouse.unwrap_or_default() {
            settings::HideMouseMode::Never => gpui::CursorHideMode::Never,
            settings::HideMouseMode::OnTyping => gpui::CursorHideMode::OnTyping,
            settings::HideMouseMode::OnTypingAndAction => gpui::CursorHideMode::OnTypingAndAction,
        })
    }
}

fn init_cursor_hide_mode(cx: &mut App) {
    let apply = |cx: &mut App| cx.set_cursor_hide_mode(CursorHideModeSetting::get_global(cx).0);
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

#[derive(Copy, Clone, Debug, settings::RegisterSetting)]
struct ReduceMotionSetting(settings::ReduceMotionMode);

impl Settings for ReduceMotionSetting {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.reduce_motion.unwrap_or_default())
    }
}

fn init_reduce_motion(cx: &mut App) {
    let apply = |cx: &mut App| {
        let reduce_motion = ReduceMotionSetting::get_global(cx).0 == settings::ReduceMotionMode::On;
        cx.set_reduce_motion(reduce_motion);
    };
    apply(cx);
    cx.observe_global::<SettingsStore>(apply).detach();
}

/// Starts watching `~/.config/zed/AGENTS.md` (or the platform equivalent) and
/// surfaces any read errors using the same notification UI as settings errors.
///
/// The file itself is loaded into [`agent_settings::UserAgentsMd`] for inclusion
/// in prompts.
pub fn watch_user_agents_md(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    struct UserAgentsMdParseError;
    let notification_id = NotificationId::unique::<UserAgentsMdParseError>();

    init_user_agents_md(fs, cx, move |state, cx| match state {
        UserAgentsMdState::Loaded(_) | UserAgentsMdState::Empty => {
            dismiss_app_notification(&notification_id, cx);
        }
        UserAgentsMdState::Error(message) => {
            let path = paths::agents_file().display().to_string();
            log::error!("Failed to load user AGENTS.md from {path}: {message}");
            let body = format!("Failed to load {path}\n{message}");
            let notification_id = notification_id.clone();
            show_app_notification(notification_id, cx, move |cx| {
                let body = body.clone();
                cx.new(|cx| MessageNotification::new(body, cx))
            });
        }
    });
}

pub fn watch_settings_files(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    MigrationNotification::set_global(cx.new(|_| MigrationNotification), cx);

    SettingsStore::update_global(cx, move |store, cx| {
        store.watch_settings_files(fs, cx, |settings_file, result, cx| {
            let is_user = matches!(settings_file, SettingsFile::User);
            let migrating_in_memory =
                matches!(&result.migration_status, MigrationStatus::Succeeded);
            notify_settings_errors(result, is_user, cx);
            if let Some(notifier) = MigrationNotification::try_global(cx) {
                notifier.update(cx, |_, cx| {
                    cx.emit(MigrationEvent::ContentChanged {
                        migration_type: MigrationType::Settings,
                        migrating_in_memory,
                    });
                });
            }
        });
    });
}

pub fn handle_keymap_file_changes(
    mut user_keymap_file_rx: mpsc::UnboundedReceiver<String>,
    user_keymap_watcher: gpui::Task<()>,
    cx: &mut App,
) {
    let (base_keymap_tx, mut base_keymap_rx) = mpsc::unbounded();
    let (keyboard_layout_tx, mut keyboard_layout_rx) = mpsc::unbounded();
    let mut old_base_keymap = *BaseKeymap::get_global(cx);
    let mut old_vim_enabled = VimModeSetting::get_global(cx).0;
    let mut old_helix_enabled = vim_mode_setting::HelixModeSetting::get_global(cx).0;
    let mut old_disable_ai = DisableAiSettings::get_global(cx).disable_ai;

    cx.observe_global::<SettingsStore>(move |cx| {
        let new_base_keymap = *BaseKeymap::get_global(cx);
        let new_vim_enabled = VimModeSetting::get_global(cx).0;
        let new_helix_enabled = vim_mode_setting::HelixModeSetting::get_global(cx).0;
        let new_disable_ai = DisableAiSettings::get_global(cx).disable_ai;

        if new_disable_ai != old_disable_ai {
            reload_menus(cx);
        }

        if new_base_keymap != old_base_keymap
            || new_vim_enabled != old_vim_enabled
            || new_helix_enabled != old_helix_enabled
            || new_disable_ai != old_disable_ai
        {
            old_base_keymap = new_base_keymap;
            old_vim_enabled = new_vim_enabled;
            old_helix_enabled = new_helix_enabled;
            old_disable_ai = new_disable_ai;

            base_keymap_tx.unbounded_send(()).unwrap();
        }
    })
    .detach();

    #[cfg(target_os = "windows")]
    {
        let mut current_layout_id = cx.keyboard_layout().id().to_string();
        cx.on_keyboard_layout_change(move |cx| {
            let next_layout_id = cx.keyboard_layout().id();
            if next_layout_id != current_layout_id {
                current_layout_id = next_layout_id.to_string();
                keyboard_layout_tx.unbounded_send(()).ok();
            }
        })
        .detach();
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut current_mapping = cx.keyboard_mapper().get_key_equivalents().cloned();
        cx.on_keyboard_layout_change(move |cx| {
            let next_mapping = cx.keyboard_mapper().get_key_equivalents();
            if current_mapping.as_ref() != next_mapping {
                current_mapping = next_mapping.cloned();
                keyboard_layout_tx.unbounded_send(()).ok();
            }
        })
        .detach();
    }

    load_default_keymap(cx);

    struct KeymapParseErrorNotification;
    let notification_id = NotificationId::unique::<KeymapParseErrorNotification>();

    cx.spawn(async move |cx| {
        let _user_keymap_watcher = user_keymap_watcher;
        let mut user_keymap_content = String::new();
        let mut migrating_in_memory = false;
        loop {
            select_biased! {
                _ = base_keymap_rx.next() => {},
                _ = keyboard_layout_rx.next() => {},
                content = user_keymap_file_rx.next() => {
                    if let Some(content) = content {
                        if let Ok(Some(migrated_content)) = migrate_keymap(&content) {
                            user_keymap_content = migrated_content;
                            migrating_in_memory = true;
                        } else {
                            user_keymap_content = content;
                            migrating_in_memory = false;
                        }
                    }
                }
            };
            cx.update(|cx| {
                if let Some(notifier) = MigrationNotification::try_global(cx) {
                    notifier.update(cx, |_, cx| {
                        cx.emit(MigrationEvent::ContentChanged {
                            migration_type: MigrationType::Keymap,
                            migrating_in_memory,
                        });
                    });
                }
                let load_result = KeymapFile::load(&user_keymap_content, cx);
                match load_result {
                    KeymapFileLoadResult::Success { key_bindings } => {
                        reload_keymaps(cx, key_bindings);
                        dismiss_app_notification(&notification_id.clone(), cx);
                    }
                    KeymapFileLoadResult::SomeFailedToLoad {
                        key_bindings,
                        error_message,
                    } => {
                        if !key_bindings.is_empty() {
                            reload_keymaps(cx, key_bindings);
                        }
                        show_keymap_file_load_error(notification_id.clone(), error_message, cx);
                    }
                    KeymapFileLoadResult::JsonParseFailure { error } => {
                        show_keymap_file_json_error(notification_id.clone(), &error, cx)
                    }
                }
            });
        }
    })
    .detach();
}

fn show_keymap_file_json_error(
    notification_id: NotificationId,
    error: &anyhow::Error,
    cx: &mut App,
) {
    let message: SharedString =
        format!("JSON parse error in keymap file. Bindings not reloaded.\n\n{error}").into();
    show_app_notification(notification_id, cx, move |cx| {
        cx.new(|cx| {
            MessageNotification::new(message.clone(), cx)
                .primary_message("Open Keymap File")
                .primary_icon(IconName::Settings)
                .primary_on_click(|window, cx| {
                    window.dispatch_action(zed_actions::OpenKeymapFile.boxed_clone(), cx);
                    cx.emit(DismissEvent);
                })
        })
    });
}

fn show_keymap_file_load_error(
    notification_id: NotificationId,
    error_message: MarkdownString,
    cx: &mut App,
) {
    show_markdown_app_notification(
        notification_id,
        error_message,
        "Open Keymap File".into(),
        |window, cx| {
            window.dispatch_action(zed_actions::OpenKeymapFile.boxed_clone(), cx);
            cx.emit(DismissEvent);
        },
        cx,
    )
}

fn show_markdown_app_notification<F>(
    notification_id: NotificationId,
    message: MarkdownString,
    primary_button_message: SharedString,
    primary_button_on_click: F,
    cx: &mut App,
) where
    F: 'static + Send + Sync + Fn(&mut Window, &mut Context<MessageNotification>),
{
    let markdown = cx.new(|cx| Markdown::new(message.0.into(), None, None, cx));
    let primary_button_on_click = Arc::new(primary_button_on_click);

    show_app_notification(notification_id, cx, move |cx| {
        let markdown = markdown.clone();
        let primary_button_message = primary_button_message.clone();
        let primary_button_on_click = primary_button_on_click.clone();

        cx.new(move |cx| {
            MessageNotification::new_from_builder(cx, move |window, cx| {
                image_cache(retain_all("notification-cache"))
                    .child(div().text_ui(cx).child(MarkdownElement::new(
                        markdown.clone(),
                        MarkdownStyle::themed(MarkdownFont::Editor, window, cx),
                    )))
                    .into_any()
            })
            .primary_message(primary_button_message)
            .primary_icon(IconName::Settings)
            .primary_on_click_arc(primary_button_on_click)
        })
    })
}

// Android has no native menus: the platform ignores `set_menus`, and the app
// draws its own menu bar (`menu_bar.rs`).
fn reload_menus(_cx: &mut App) {}

fn reload_keymaps(cx: &mut App, mut user_key_bindings: Vec<KeyBinding>) {
    cx.clear_key_bindings();
    load_default_keymap(cx);

    for key_binding in &mut user_key_bindings {
        key_binding.set_meta(KeybindSource::User.meta());
    }
    cx.bind_keys(filter_disabled_ai_bindings(user_key_bindings, cx));
    reload_menus(cx);
    // On Windows, this is set in the `update_jump_list` method of the `HistoryManager`.
    #[cfg(not(target_os = "windows"))]
    cx.set_dock_menu(vec![gpui::MenuItem::action(
        "New Window",
        workspace::NewWindow,
    )]);
    // todo: nicer api here?
    keymap_editor::KeymapEventChannel::trigger_keymap_changed(cx);
}

pub fn load_default_keymap(cx: &mut App) {
    let base_keymap = *BaseKeymap::get_global(cx);
    if base_keymap == BaseKeymap::None {
        return;
    }

    cx.bind_keys(filter_disabled_ai_bindings(
        load_asset(DEFAULT_KEYMAP_PATH, Some(KeybindSource::Default), cx).unwrap(),
        cx,
    ));

    if let Some(asset_path) = base_keymap.asset_path() {
        cx.bind_keys(filter_disabled_ai_bindings(
            load_asset(asset_path, Some(KeybindSource::Base), cx).unwrap(),
            cx,
        ));
    }

    if VimModeSetting::get_global(cx).0 || vim_mode_setting::HelixModeSetting::get_global(cx).0 {
        cx.bind_keys(filter_disabled_ai_bindings(
            load_asset(VIM_KEYMAP_PATH, Some(KeybindSource::Vim), cx).unwrap(),
            cx,
        ));
    }

    cx.bind_keys(
        load_asset(
            SPECIFIC_OVERRIDES_KEYMAP_PATH,
            Some(KeybindSource::Default),
            cx,
        )
        .unwrap(),
    );
}

/// `KeymapFile::load_asset`, but keeping the bindings that load when others
/// don't. Zed's keymaps bind actions of crates the app doesn't build, such as
/// the debugger's, and `load_asset` fails on those.
fn load_asset(
    asset_path: &str,
    source: Option<KeybindSource>,
    cx: &App,
) -> anyhow::Result<Vec<KeyBinding>> {
    let mut key_bindings = KeymapFile::load_asset_allow_partial_failure(asset_path, cx)?;
    if let Some(source) = source {
        for key_binding in &mut key_bindings {
            key_binding.set_meta(source.meta());
        }
    }
    Ok(key_bindings)
}

/// Namespaces of actions that are part of an AI feature. When the user opts out
/// of AI via the `disable_ai` setting, bindings to these actions are dropped so
/// that lower-precedence editor defaults (e.g. `editor::NewlineBelow` for
/// `ctrl-enter`) can fire instead of being shadowed by an action whose handler
/// silently no-ops.
const AI_ACTION_NAMESPACES: &[&str] = &[
    "acp::",
    "agent::",
    "assistant::",
    "edit_prediction::",
    "inline_assistant::",
    "zeta::",
];

fn is_ai_keybinding(binding: &KeyBinding) -> bool {
    let name = binding.action().name();
    AI_ACTION_NAMESPACES
        .iter()
        .any(|namespace| name.starts_with(namespace))
}

fn filter_disabled_ai_bindings(bindings: Vec<KeyBinding>, cx: &App) -> Vec<KeyBinding> {
    if !DisableAiSettings::get_global(cx).disable_ai {
        return bindings;
    }
    bindings
        .into_iter()
        .filter(|binding| !is_ai_keybinding(binding))
        .collect()
}

pub fn open_new_ssh_project_from_project(
    workspace: &mut Workspace,
    paths: Vec<PathBuf>,
    create_new_window: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    let app_state = workspace.app_state().clone();
    let Some(ssh_client) = workspace.project().read(cx).remote_client() else {
        return Task::ready(Err(anyhow::anyhow!("Not an ssh project")));
    };
    let connection_options = ssh_client.read(cx).connection_options();
    let requesting_window = if create_new_window {
        None
    } else {
        window.window_handle().downcast::<MultiWorkspace>()
    };
    cx.spawn_in(window, async move |_, cx| {
        open_remote_project(
            connection_options,
            paths,
            app_state,
            workspace::OpenOptions {
                workspace_matching: workspace::WorkspaceMatching::None,
                requesting_window,
                ..Default::default()
            },
            cx,
        )
        .await
        .map(|_| ())
    })
}

fn open_project_settings_file(
    workspace: &mut Workspace,
    _: &OpenProjectSettingsFile,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(task) = open_local_file(
        workspace,
        local_settings_file_relative_path(),
        initial_project_settings_content(),
        window,
        cx,
    ) {
        task.detach_and_log_err(cx);
    }
}

fn open_project_tasks_file(
    workspace: &mut Workspace,
    _: &OpenProjectTasks,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(task) = open_local_file(
        workspace,
        local_tasks_file_relative_path(),
        initial_tasks_content(),
        window,
        cx,
    ) {
        task.detach_and_log_err(cx);
    }
}

fn open_worktree_setup_tasks_file(
    workspace: &mut Workspace,
    _: &zed_actions::OpenWorktreeSetupTasks,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Kept harmless on purpose: tasks with the `create_worktree` hook run automatically
    // when a worktree is created, so the example must be safe to save unedited.
    const WORKTREE_SETUP_TASK_EXAMPLE: &str = r#"  {
    // Runs automatically after Zed creates a new git worktree.
    // $ZED_WORKTREE_ROOT is the new worktree's root directory, and
    // $ZED_MAIN_GIT_WORKTREE is the original repository's working directory.
    "label": "Set up new worktree",
    "command": "echo \"Setting up $ZED_WORKTREE_ROOT — edit this command\"",
    "cwd": "$ZED_WORKTREE_ROOT",
    "hooks": ["create_worktree"]
  }"#;

    let Some(open_task) = open_local_file(
        workspace,
        local_tasks_file_relative_path(),
        settings::initial_worktree_setup_tasks_content(),
        window,
        cx,
    ) else {
        return;
    };

    cx.spawn_in(window, async move |_, cx| {
        let editor = open_task.await?;
        editor.update_in(cx, |editor, window, cx| {
            // Skip insertion if the file already mentions the hook (even in a comment,
            // like the seeded template's example — uncommenting it beats duplicating it).
            // `create_git_worktree` is a serde alias for the same hook.
            let text = editor.text(cx);
            if text.contains("create_worktree") || text.contains("create_git_worktree") {
                return anyhow::Ok(());
            }
            tasks_ui::insert_task_json_into_editor(
                editor,
                WORKTREE_SETUP_TASK_EXAMPLE.to_string(),
                window,
                cx,
            )
        })?
    })
    .detach_and_log_err(cx);
}

fn open_project_debug_tasks_file(
    workspace: &mut Workspace,
    _: &zed_actions::OpenProjectDebugTasks,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(task) = open_local_file(
        workspace,
        local_debug_file_relative_path(),
        initial_local_debug_tasks_content(),
        window,
        cx,
    ) {
        task.detach_and_log_err(cx);
    }
}

fn open_local_file(
    workspace: &mut Workspace,
    settings_relative_path: &'static RelPath,
    initial_contents: Cow<'static, str>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<gpui::Task<anyhow::Result<Entity<Editor>>>> {
    let project = workspace.project().clone();
    let worktree = project
        .read(cx)
        .visible_worktrees(cx)
        .find_map(|tree| tree.read(cx).root_entry()?.is_dir().then_some(tree));
    if let Some(worktree) = worktree {
        let tree_id = worktree.read(cx).id();
        Some(cx.spawn_in(window, async move |workspace, cx| {
            // Check if the file actually exists on disk (even if it's excluded from worktree)
            let file_exists = {
                let full_path = worktree.read_with(cx, |tree, _| {
                    tree.abs_path().join(settings_relative_path.as_std_path())
                });

                let fs = project.read_with(cx, |project, _| project.fs().clone());

                fs.metadata(&full_path)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|metadata| !metadata.is_dir && !metadata.is_fifo)
            };

            if !file_exists {
                if let Some(dir_path) = settings_relative_path.parent()
                    && worktree.read_with(cx, |tree, _| tree.entry_for_path(dir_path).is_none())
                {
                    project
                        .update(cx, |project, cx| {
                            project.create_entry((tree_id, dir_path), true, cx)
                        })
                        .await
                        .context("worktree was removed")?;
                }

                if worktree.read_with(cx, |tree, _| {
                    tree.entry_for_path(settings_relative_path).is_none()
                }) {
                    project
                        .update(cx, |project, cx| {
                            project.create_entry((tree_id, settings_relative_path), false, cx)
                        })
                        .await
                        .context("worktree was removed")?;
                }
            }

            let editor = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path((tree_id, settings_relative_path), None, true, window, cx)
                })?
                .await?
                .downcast::<Editor>()
                .context("unexpected item type: expected editor item")?;

            editor.update(cx, |editor, cx| {
                if let Some(buffer) = editor.buffer().read(cx).as_singleton()
                    && buffer.read(cx).is_empty()
                {
                    buffer.update(cx, |buffer, cx| {
                        buffer.edit([(0..0, initial_contents)], None, cx)
                    });
                }
            });

            anyhow::Ok(editor)
        }))
    } else {
        struct NoOpenFolders;

        workspace.show_notification(NotificationId::unique::<NoOpenFolders>(), cx, |cx| {
            cx.new(|cx| MessageNotification::new("This project has no folders open.", cx))
        });
        None
    }
}

fn open_bundled_file(
    workspace: &mut Workspace,
    text: Cow<'static, str>,
    title: &'static str,
    language: &'static str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<Editor>(cx).find(|editor| {
        editor.read_with(cx, |editor, cx| {
            editor.read_only(cx)
                && editor.title(cx).as_ref() == title
                && editor
                    .buffer()
                    .read(cx)
                    .as_singleton()
                    .is_some_and(|buffer| buffer.read(cx).file().is_none())
        })
    });
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }

    let language = workspace.app_state().languages.language_for_name(language);
    cx.spawn_in(window, async move |workspace, cx| {
        let language = language.await.log_err();
        workspace
            .update_in(cx, move |workspace, window, cx| {
                let project = workspace.project().clone();
                let buffer = project.update(cx, move |project, cx| {
                    project.create_buffer(language, false, cx)
                });
                cx.spawn_in(window, async move |workspace, cx| {
                    let buffer = buffer.await?;
                    buffer.update(cx, |buffer, cx| {
                        buffer.set_text(text.into_owned(), cx);
                        buffer.set_capability(Capability::ReadOnly, cx);
                    });
                    let buffer =
                        cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.into()));
                    workspace.update_in(cx, |workspace, window, cx| {
                        workspace.add_item_to_active_pane(
                            Box::new(cx.new(|cx| {
                                let mut editor = Editor::for_multibuffer(
                                    buffer,
                                    Some(project.clone()),
                                    window,
                                    cx,
                                );
                                editor.set_read_only(true);
                                editor.set_should_serialize(false, cx);
                                editor.set_breadcrumb_header(title.into());
                                editor
                            })),
                            None,
                            true,
                            window,
                            cx,
                        )
                    })
                })
            })?
            .await
    })
    .detach_and_log_err(cx);
}

fn open_settings_file(
    abs_path: &'static Path,
    default_content: impl FnOnce() -> Rope + Send + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.spawn_in(window, async move |workspace, cx| {
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.with_local_or_wsl_workspace(window, cx, move |workspace, window, cx| {
                    let project = workspace.project().clone();

                    cx.spawn_in(window, async move |workspace, cx| {
                        let config_dir = project
                            .update(cx, |project, cx| {
                                project.try_windows_path_to_wsl(paths::config_dir().as_path(), cx)
                            })
                            .await?;
                        // Set up a dedicated worktree for settings, since
                        // otherwise we're dropping and re-starting LSP servers
                        // for each file inside on every settings file
                        // close/open

                        // TODO: Do note that all other external files (e.g.
                        // drag and drop from OS) still have their worktrees
                        // released on file close, causing LSP servers'
                        // restarts.
                        let (_worktree, _) = project
                            .update(cx, |project, cx| {
                                project.find_or_create_worktree(&config_dir, false, cx)
                            })
                            .await?;

                        workspace
                            .update_in(cx, |_, window, cx| {
                                create_and_open_local_file(abs_path, window, cx, default_content)
                            })?
                            .await?;
                        anyhow::Ok(())
                    })
                })
            })?
            .await?
            .await?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Eagerly loads the active theme and icon theme based on the selections in the
/// theme settings.
///
/// This fast path exists to load these themes as soon as possible so the user
/// doesn't see the default themes while waiting on extensions to load.
pub(crate) fn eager_load_active_theme_and_icon_theme(fs: Arc<dyn Fs>, cx: &mut App) {
    let extension_store = ExtensionStore::global(cx);
    let theme_registry = ThemeRegistry::global(cx);
    let theme_settings = ThemeSettings::get_global(cx);
    let appearance = SystemAppearance::global(cx).0;

    enum LoadTarget {
        Theme(PathBuf),
        IconTheme((PathBuf, PathBuf)),
    }

    let theme_name = theme_settings.theme.name(appearance);
    let icon_theme_name = theme_settings.icon_theme.name(appearance);
    let themes_to_load = [
        theme_registry
            .get(&theme_name.0)
            .is_err()
            .then(|| {
                extension_store
                    .read(cx)
                    .path_to_extension_theme(&theme_name.0)
            })
            .flatten()
            .map(LoadTarget::Theme),
        theme_registry
            .get_icon_theme(&icon_theme_name.0)
            .is_err()
            .then(|| {
                extension_store
                    .read(cx)
                    .path_to_extension_icon_theme(&icon_theme_name.0)
            })
            .flatten()
            .map(LoadTarget::IconTheme),
    ];

    enum ReloadTarget {
        Theme,
        IconTheme,
    }

    let executor = cx.background_executor();
    let reload_tasks = parking_lot::Mutex::new(Vec::with_capacity(themes_to_load.len()));

    let mut themes_to_load = themes_to_load.into_iter().flatten().peekable();

    if themes_to_load.peek().is_none() {
        return;
    }

    cx.foreground_executor().block_on(executor.scoped(|scope| {
        for load_target in themes_to_load {
            let theme_registry = &theme_registry;
            let reload_tasks = &reload_tasks;
            let fs = fs.clone();

            scope.spawn(async move {
                match load_target {
                    LoadTarget::Theme(theme_path) => {
                        if let Some(bytes) = fs
                            .load_bytes(&theme_path)
                            .await
                            .with_context(|| format!("loading theme bytes from {theme_path:?}"))
                            .log_err()
                            && load_user_theme(theme_registry, &bytes).log_err().is_some()
                        {
                            reload_tasks.lock().push(ReloadTarget::Theme);
                        }
                    }
                    LoadTarget::IconTheme((icon_theme_path, icons_root_path)) => {
                        if let Some(bytes) = fs
                            .load_bytes(&icon_theme_path)
                            .await
                            .with_context(|| format!("loading icon bytes from {icon_theme_path:?}"))
                            .log_err()
                            && theme_registry
                                .load_icon_theme(&icon_theme_path, &icons_root_path, bytes)
                                .log_err()
                                .is_some()
                        {
                            reload_tasks.lock().push(ReloadTarget::IconTheme);
                        }
                    }
                }
            });
        }
    }));

    for reload_target in reload_tasks.into_inner() {
        match reload_target {
            ReloadTarget::Theme => theme_settings::reload_theme(cx),
            ReloadTarget::IconTheme => theme_settings::reload_icon_theme(cx),
        };
    }
}
