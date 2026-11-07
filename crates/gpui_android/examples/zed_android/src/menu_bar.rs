//! The application menus on Android, which has no system menu bar. On Linux
//! and Windows Zed draws them through the `title_bar` crate, which brings
//! calls and sign-in along, so the app draws them itself. [`app_menus`] is a
//! copy of `crates/zed/src/zed/app_menus.rs` to diff at each Zed sync.

use gpui::{
    App, Context, FocusHandle, Focusable, IntoElement, Menu, MenuItem, OsAction, OwnedMenuItem,
    Render, SharedString, WeakEntity, Window, actions,
};
use project::DisableAiSettings;
use settings::Settings as _;
use terminal_view::terminal_panel;
use ui::prelude::*;
use ui::{ContextMenu, PopoverMenu};
use util::ResultExt as _;
use workspace::Workspace;
use zed_actions::{Quit, assistant, debug_panel, git_panel, project_panel};

actions!(
    zed_android,
    [
        /// Toggles the always-on Android application menu bar.
        ToggleAppMenuBar,
        /// Keeps Zdroid running in the background behind an ongoing
        /// notification, so terminal, SSH and agent sessions stay connected.
        ToggleBackgroundMode
    ]
);

pub fn register_actions(cx: &mut App) {
    cx.on_action(|_: &ToggleBackgroundMode, _: &mut App| {
        let enabled = !gpui_android::background_mode::is_enabled();
        gpui_android::background_mode::set_enabled(enabled).log_err();
    });
}

/// Bound by `zed::load_default_keymap`, which runs again whenever the keymaps
/// reload and clears every binding first.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    vec![gpui::KeyBinding::new("ctrl-alt-m", ToggleAppMenuBar, None)]
}

pub struct MenuBar {
    hidden: bool,
    workspace: WeakEntity<Workspace>,
    menu_names: Vec<SharedString>,
}

impl MenuBar {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &App) -> Self {
        Self {
            hidden: false,
            workspace,
            menu_names: app_menus(cx).into_iter().map(|menu| menu.name).collect(),
        }
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.hidden = !self.hidden;
        cx.notify();
    }
}

impl Render for MenuBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.hidden {
            return div().into_any_element();
        }

        let buttons = self.menu_names.iter().enumerate().map(|(index, name)| {
            let workspace = self.workspace.clone();
            PopoverMenu::new(("zed-android-menubar", index))
                .menu(move |window, cx| {
                    let menu = app_menus(cx).into_iter().nth(index)?.owned();
                    // Actions go to the active item's focus handle, because
                    // the popover has focus when they fire, and looking up
                    // the focused element then misses the editor.
                    let active_focus = workspace
                        .read_with(cx, |workspace, cx| {
                            let pane = workspace.active_pane().read(cx);
                            pane.active_item()
                                .map(|item| item.item_focus_handle(cx))
                                .unwrap_or_else(|| pane.focus_handle(cx))
                        })
                        .ok();
                    Some(ContextMenu::build(window, cx, |menu_builder, _, _| {
                        let menu_builder = match &active_focus {
                            Some(focus) => menu_builder.context(focus.clone()),
                            None => menu_builder,
                        };
                        add_items(menu_builder, menu.items, active_focus)
                    }))
                })
                .trigger(
                    Button::new(("zed-android-menubar-trigger", index), name.clone())
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle),
                )
                .into_any_element()
        });

        h_flex()
            .w_full()
            .h_6()
            .px_2()
            .gap_3()
            .items_center()
            .bg(cx.theme().colors().title_bar_background)
            .children(buttons)
            .into_any_element()
    }
}

fn add_items(
    mut menu: ContextMenu,
    items: Vec<OwnedMenuItem>,
    focus: Option<FocusHandle>,
) -> ContextMenu {
    for item in items {
        menu = match item {
            OwnedMenuItem::Separator => menu.separator(),
            OwnedMenuItem::Action { name, action, .. } => {
                let dispatch_via = focus.clone();
                let handler_action = action.boxed_clone();
                menu.entry(name, Some(action), move |window, cx| match &dispatch_via {
                    Some(handle) => handle.dispatch_action(handler_action.as_ref(), window, cx),
                    None => window.dispatch_action(handler_action.boxed_clone(), cx),
                })
            }
            OwnedMenuItem::Submenu(submenu) => {
                let focus = focus.clone();
                menu.submenu(submenu.name.clone(), move |sub_menu, _window, _cx| {
                    add_items(sub_menu, submenu.items.clone(), focus.clone())
                })
            }
            OwnedMenuItem::SystemMenu(_) => menu,
        };
    }
    menu
}

/// Upstream's `app_menus`, without what Android doesn't have: the collab panel
/// and calls, the About window, the CLI, macOS's items, window controls, the
/// GPUI inspector and the Help menu's links to Zed. `Zdroid` adds the
/// background toggle and the welcome pages.
fn app_menus(cx: &App) -> Vec<Menu> {
    let mut view_items = vec![
        MenuItem::action(
            "Zoom In",
            zed_actions::IncreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "Zoom Out",
            zed_actions::DecreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "Reset Zoom",
            zed_actions::ResetBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "Reset All Zoom",
            zed_actions::ResetAllZoom { persist: false },
        ),
        MenuItem::separator(),
        MenuItem::action("Toggle Left Dock", workspace::ToggleLeftDock),
        MenuItem::action("Toggle Right Dock", workspace::ToggleRightDock),
        MenuItem::action("Toggle Bottom Dock", workspace::ToggleBottomDock),
        MenuItem::action("Toggle All Docks", workspace::ToggleAllDocks),
        MenuItem::submenu(Menu {
            name: "Editor Layout".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Split Up", workspace::SplitUp::default()),
                MenuItem::action("Split Down", workspace::SplitDown::default()),
                MenuItem::action("Split Left", workspace::SplitLeft::default()),
                MenuItem::action("Split Right", workspace::SplitRight::default()),
            ],
        }),
        MenuItem::separator(),
        MenuItem::action("Project Panel", project_panel::ToggleFocus),
        MenuItem::action("Outline Panel", outline_panel::ToggleFocus),
        MenuItem::action("Terminal Panel", terminal_panel::Toggle),
        MenuItem::action("Debugger Panel", debug_panel::ToggleFocus),
    ];

    if !DisableAiSettings::get_global(cx).disable_ai {
        view_items.push(MenuItem::action("Agent Panel", assistant::ToggleFocus));
    }

    view_items.extend([
        MenuItem::action("Git Panel", git_panel::ToggleFocus),
        MenuItem::separator(),
        MenuItem::action("Diagnostics", diagnostics::Deploy),
    ]);

    vec![
        Menu {
            name: "Zdroid".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Check for Updates", auto_update::Check),
                MenuItem::separator(),
                MenuItem::submenu(Menu::new("Settings").items([
                    MenuItem::action("Open Settings", zed_actions::OpenSettings),
                    MenuItem::action("Open Settings File", zed_actions::OpenSettingsFile),
                    MenuItem::action("Open Project Settings", zed_actions::OpenProjectSettings),
                    MenuItem::action(
                        "Open Project Settings File",
                        crate::zed::OpenProjectSettingsFile,
                    ),
                    MenuItem::action("Open Default Settings", crate::zed::OpenDefaultSettings),
                    MenuItem::separator(),
                    MenuItem::action("Open Keymap", zed_actions::OpenKeymap),
                    MenuItem::action("Open Keymap File", zed_actions::OpenKeymapFile),
                    MenuItem::action("Open Default Key Bindings", zed_actions::OpenDefaultKeymap),
                    MenuItem::separator(),
                    MenuItem::action(
                        "Select Theme...",
                        zed_actions::theme_selector::Toggle::default(),
                    ),
                    MenuItem::action(
                        "Select Icon Theme...",
                        zed_actions::icon_theme_selector::Toggle::default(),
                    ),
                ])),
                MenuItem::separator(),
                MenuItem::action("Extensions", zed_actions::Extensions::default()),
                MenuItem::action(
                    if gpui_android::background_mode::is_enabled() {
                        "Stop Running in Background"
                    } else {
                        "Keep Running in Background"
                    },
                    ToggleBackgroundMode,
                ),
                MenuItem::separator(),
                MenuItem::action("Welcome", workspace::welcome::ShowWelcome),
                MenuItem::action("Onboarding", zed_actions::OpenOnboarding),
                MenuItem::separator(),
                MenuItem::action("Quit", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            disabled: false,
            items: vec![
                MenuItem::action("New", workspace::NewFile),
                MenuItem::separator(),
                MenuItem::action("Open File...", workspace::OpenFiles),
                MenuItem::action("Open Folder...", workspace::Open::default()),
                // Android opens these in the current window, unlike upstream's
                // defaults; a new window is a separate task in Recents.
                MenuItem::action(
                    "Open Recent…",
                    zed_actions::OpenRecent {
                        create_new_window: Some(false),
                    },
                ),
                MenuItem::action(
                    "Open Remote…",
                    zed_actions::OpenRemote {
                        from_existing_connection: false,
                        create_new_window: Some(false),
                    },
                ),
                MenuItem::separator(),
                MenuItem::action("Add Folder to Project…", workspace::AddFolderToProject),
                MenuItem::separator(),
                MenuItem::action("Save", workspace::Save { save_intent: None }),
                MenuItem::action("Save As…", workspace::SaveAs),
                MenuItem::action("Save All", workspace::SaveAll { save_intent: None }),
                MenuItem::separator(),
                MenuItem::action(
                    "Close Editor",
                    workspace::CloseActiveItem {
                        save_intent: None,
                        close_pinned: true,
                    },
                ),
                MenuItem::action("Close Project", workspace::CloseProject),
            ],
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: vec![
                MenuItem::os_action("Undo", editor::actions::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", editor::actions::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", editor::actions::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", editor::actions::Copy, OsAction::Copy),
                MenuItem::action("Copy and Trim", editor::actions::CopyAndTrim),
                MenuItem::os_action("Paste", editor::actions::Paste, OsAction::Paste),
                MenuItem::separator(),
                MenuItem::action("Find", search::buffer_search::Deploy::find()),
                MenuItem::action("Find in Project", workspace::DeploySearch::default()),
                MenuItem::separator(),
                MenuItem::action(
                    "Toggle Line Comment",
                    editor::actions::ToggleComments::default(),
                ),
            ],
        },
        Menu {
            name: "Selection".into(),
            disabled: false,
            items: vec![
                MenuItem::os_action(
                    "Select All",
                    editor::actions::SelectAll,
                    OsAction::SelectAll,
                ),
                MenuItem::action("Expand Selection", editor::actions::SelectLargerSyntaxNode),
                MenuItem::action("Shrink Selection", editor::actions::SelectSmallerSyntaxNode),
                MenuItem::action("Select Next Sibling", editor::actions::SelectNextSyntaxNode),
                MenuItem::action(
                    "Select Previous Sibling",
                    editor::actions::SelectPreviousSyntaxNode,
                ),
                MenuItem::separator(),
                MenuItem::action(
                    "Add Cursor Above",
                    editor::actions::AddSelectionAbove {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    "Add Cursor Below",
                    editor::actions::AddSelectionBelow {
                        skip_soft_wrap: true,
                    },
                ),
                MenuItem::action(
                    "Select Next Occurrence",
                    editor::actions::SelectNext {
                        replace_newest: false,
                    },
                ),
                MenuItem::action(
                    "Select Previous Occurrence",
                    editor::actions::SelectPrevious {
                        replace_newest: false,
                    },
                ),
                MenuItem::action("Select All Occurrences", editor::actions::SelectAllMatches),
                MenuItem::separator(),
                MenuItem::action("Move Line Up", editor::actions::MoveLineUp),
                MenuItem::action("Move Line Down", editor::actions::MoveLineDown),
                MenuItem::action("Duplicate Selection", editor::actions::DuplicateLineDown),
            ],
        },
        Menu {
            name: "View".into(),
            disabled: false,
            items: view_items,
        },
        Menu {
            name: "Go".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Back", workspace::GoBack),
                MenuItem::action("Forward", workspace::GoForward),
                MenuItem::separator(),
                MenuItem::action("Command Palette...", zed_actions::command_palette::Toggle),
                MenuItem::separator(),
                MenuItem::action("Go to File...", workspace::ToggleFileFinder::default()),
                MenuItem::action(
                    "Go to Symbol in Editor...",
                    zed_actions::outline::ToggleOutline,
                ),
                MenuItem::action("Go to Line/Column...", editor::actions::ToggleGoToLine),
                MenuItem::separator(),
                MenuItem::action(
                    "Go to Definition",
                    editor::actions::GoToDefinition::default(),
                ),
                MenuItem::action(
                    "Go to Declaration",
                    editor::actions::GoToDeclaration::default(),
                ),
                MenuItem::action(
                    "Go to Type Definition",
                    editor::actions::GoToTypeDefinition::default(),
                ),
                MenuItem::action(
                    "Find All References",
                    editor::actions::FindAllReferences::default(),
                ),
                MenuItem::action("Show Incoming Calls", call_hierarchy::ShowIncomingCalls),
                MenuItem::action("Show Outgoing Calls", call_hierarchy::ShowOutgoingCalls),
                MenuItem::separator(),
                MenuItem::action("Next Problem", editor::actions::GoToDiagnostic::default()),
                MenuItem::action(
                    "Previous Problem",
                    editor::actions::GoToPreviousDiagnostic::default(),
                ),
            ],
        },
        Menu {
            name: "Run".into(),
            disabled: false,
            items: vec![
                MenuItem::action(
                    "Spawn Task",
                    zed_actions::Spawn::ViaModal {
                        reveal_target: None,
                    },
                ),
                MenuItem::action("Start Debugger", debugger_ui::Start),
                MenuItem::separator(),
                MenuItem::action("Edit tasks.json…", zed_actions::OpenProjectTasks),
                MenuItem::action("Edit debug.json…", zed_actions::OpenProjectDebugTasks),
                MenuItem::separator(),
                MenuItem::action("Continue", debugger_ui::Continue),
                MenuItem::action("Step Over", debugger_ui::StepOver),
                MenuItem::action("Step Into", debugger_ui::StepInto),
                MenuItem::action("Step Out", debugger_ui::StepOut),
                MenuItem::separator(),
                MenuItem::action("Toggle Breakpoint", editor::actions::ToggleBreakpoint),
                MenuItem::action("Edit Breakpoint", editor::actions::EditLogBreakpoint),
                MenuItem::action("Clear All Breakpoints", debugger_ui::ClearAllBreakpoints),
            ],
        },
    ]
}
