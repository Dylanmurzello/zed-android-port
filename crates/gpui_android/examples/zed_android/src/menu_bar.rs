//! Always-on application menu bar for Android.
//!
//! Production zed builds a `Vec<gpui::Menu>` via `app_menus()`, hands it to
//! `cx.set_menus(...)`, and lets the platform render it (macOS native menu
//! bar; Linux/Windows `title_bar` crate). Android has no native menu
//! surface and `title_bar` drags in `auto_update`/`call`/`livekit_client`/
//! `git_ui` we don't ship — so we render our own.
//!
//! The bar lives as the workspace's `titlebar_item` so it sits above the
//! tab bar / dock chrome, can be toggled hidden via the rightmost
//! affordance, and dispatches the same actions production's menu items
//! dispatch (so behaviour is identical to clicking `Open Project` from
//! macOS' File menu).
use std::sync::Mutex;

use gpui::{
    Action, App, Context, Entity, Focusable, IntoElement, Render, WeakEntity, Window, actions,
};
use ui::prelude::*;
use ui::{ContextMenu, PopoverMenu, PopoverMenuHandle};
use workspace::Workspace;

actions!(
    zed_android,
    [
        /// Toggles the always-on Android application menu bar.
        ToggleAppMenuBar
    ]
);

static ACTIVE_MENU_BAR: Mutex<Option<WeakEntity<MenuBar>>> = Mutex::new(None);

pub fn register_actions(cx: &mut App) {
    cx.on_action(|_: &ToggleAppMenuBar, cx: &mut App| {
        let weak = ACTIVE_MENU_BAR.lock().ok().and_then(|g| g.clone());
        if let Some(weak) = weak {
            weak.update(cx, |bar, cx| {
                bar.hidden = !bar.hidden;
                cx.notify();
            })
            .ok();
        }
    });
    cx.bind_keys([gpui::KeyBinding::new(
        "ctrl-alt-m",
        ToggleAppMenuBar,
        None,
    )]);
}

pub struct MenuBar {
    pub hidden: bool,
    workspace: WeakEntity<Workspace>,
    menus: Vec<MenuDefinition>,
    handles: Vec<PopoverMenuHandle<ContextMenu>>,
}

pub fn register(entity: &Entity<MenuBar>) {
    if let Ok(mut slot) = ACTIVE_MENU_BAR.lock() {
        *slot = Some(entity.downgrade());
    }
}

struct MenuDefinition {
    title: &'static str,
    items: fn() -> Vec<MenuEntry>,
}

enum MenuEntry {
    Action(&'static str, Box<dyn Action>),
    Separator,
    Submenu(&'static str, fn() -> Vec<MenuEntry>),
}

impl MenuBar {
    pub fn new(workspace: WeakEntity<Workspace>, _cx: &mut Context<Self>) -> Self {
        let menus = app_menu_definitions();
        let handles = menus
            .iter()
            .map(|_| PopoverMenuHandle::default())
            .collect();
        Self {
            hidden: false,
            workspace,
            menus,
            handles,
        }
    }
}

impl Render for MenuBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.hidden {
            // Render nothing — the workspace's titlebar slot collapses
            // and the editor / dock chrome occupies the freed pixels.
            // Restore via the `ToggleAppMenuBar` action (Ctrl+Alt+M, or
            // dispatch from the command palette).
            return div().w_0().h_0().into_any_element();
        }

        let mut buttons: Vec<gpui::AnyElement> = Vec::with_capacity(self.menus.len());
        for (idx, menu) in self.menus.iter().enumerate() {
            let title = menu.title;
            let items = menu.items;
            let handle = self.handles[idx].clone();
            // The action context is the workspace's focus handle: when the
            // menu fires `Save` / `Cut` / etc., it focuses this handle and
            // dispatches there so workspace's `register_action(...)` and
            // the active editor's element-level handlers actually run.
            // Without this, dispatch goes through whatever stray focus
            // remains after the popover dismisses and the action
            // silently misses its target.
            let workspace = self.workspace.clone();
            buttons.push(
                PopoverMenu::new(("zed-android-menubar", idx))
                    .menu({
                        let workspace = workspace.clone();
                        move |window, cx| {
                            // Snapshot of the active pane's item right now —
                            // this gives us the editor's focus node (Pane
                            // forwards focus to its active item). We
                            // dispatch via this handle directly rather than
                            // relying on `window.dispatch_action`, which
                            // looks up the *currently* focused element in
                            // the rendered frame. After the popover takes
                            // focus and dismisses, that lookup misses the
                            // editor and editor::Cut / Copy / Paste / etc.
                            // never reach a handler.
                            let active_focus = workspace
                                .read_with(cx, |workspace, cx| {
                                    let pane = workspace.active_pane().read(cx);
                                    pane.active_item()
                                        .map(|item| item.item_focus_handle(cx))
                                        .unwrap_or_else(|| pane.focus_handle(cx))
                                })
                                .ok();
                            Some(ContextMenu::build(window, cx, |mut menu, _, _| {
                                if let Some(ref handle) = active_focus {
                                    menu = menu.context(handle.clone());
                                }
                                menu = build_menu_entries(
                                    menu,
                                    items(),
                                    active_focus.clone(),
                                );
                                menu
                            }))
                        }
                    })
                    .with_handle(handle)
                    .trigger(
                        Button::new(("zed-android-menubar-trigger", idx), title)
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle),
                    )
                    .into_any_element(),
            );
        }

        // The visibility-toggle chevron now lives on the title bar
        // below us — single-tap toggles, two-finger / right-click opens
        // the Settings/Keymap/Themes/… dropdown. See `title_bar.rs`.
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

fn build_menu_entries(
    mut menu: ContextMenu,
    entries: Vec<MenuEntry>,
    active_focus: Option<gpui::FocusHandle>,
) -> ContextMenu {
    for entry in entries {
        match entry {
            MenuEntry::Separator => {
                menu = menu.separator();
            }
            MenuEntry::Action(label, action) => {
                let dispatch_via = active_focus.clone();
                let action_for_handler = action.boxed_clone();
                menu = menu.entry(label, Some(action), move |window, cx| {
                    let action = action_for_handler.boxed_clone();
                    if let Some(ref handle) = dispatch_via {
                        handle.dispatch_action(action.as_ref(), window, cx);
                    } else {
                        window.dispatch_action(action, cx);
                    }
                });
            }
            MenuEntry::Submenu(label, items_fn) => {
                let active_focus = active_focus.clone();
                menu = menu.submenu(label, move |sub_menu, _window, _cx| {
                    build_menu_entries(sub_menu, items_fn(), active_focus.clone())
                });
            }
        }
    }
    menu
}

/// Mirrors production's `app_menus()` (crates/zed/src/zed/app_menus.rs)
/// minus entries whose actions live in crates we don't ship on Android
/// (auto_update, debugger, collab_panel, install_cli, etc.). The
/// nested Settings submenu under Zed mirrors production verbatim — the
/// handlers it dispatches to are registered globally by
/// settings_ui::init / keymap_editor::init / theme_selector::init in
/// our boot chain (lib.rs).
fn app_menu_definitions() -> Vec<MenuDefinition> {
    vec![
        MenuDefinition {
            title: "Zdroid",
            items: zed_menu_items,
        },
        MenuDefinition {
            title: "File",
            items: file_menu_items,
        },
        MenuDefinition {
            title: "Edit",
            items: edit_menu_items,
        },
        MenuDefinition {
            title: "Selection",
            items: selection_menu_items,
        },
        MenuDefinition {
            title: "View",
            items: view_menu_items,
        },
        MenuDefinition {
            title: "Go",
            items: go_menu_items,
        },
    ]
}

fn zed_menu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Submenu("Settings", zed_settings_submenu_items),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Check for Updates",
            Box::new(auto_update::Check),
        ),
        MenuEntry::Action(
            "Extensions",
            Box::new(zed_actions::Extensions::default()),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Welcome",
            Box::new(workspace::welcome::ShowWelcome),
        ),
        MenuEntry::Action(
            "Onboarding",
            Box::new(zed_actions::OpenOnboarding),
        ),
        MenuEntry::Separator,
        MenuEntry::Action("Quit", Box::new(zed_actions::Quit)),
    ]
}

/// Settings submenu nested under Zed — mirrors production
/// `crates/zed/src/zed/app_menus.rs:69-87`. All ten entries the user
/// listed: file/default-variant handlers landed in `editor` and
/// `workspace` (see `editor::init_bundled_file_actions`,
/// `workspace::init_settings_file_actions`,
/// `editor::open_project_settings_file`).
fn zed_settings_submenu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Action("Open Settings", Box::new(zed_actions::OpenSettings)),
        MenuEntry::Action(
            "Open Settings File",
            Box::new(zed_actions::OpenSettingsFile),
        ),
        MenuEntry::Action(
            "Open Project Settings",
            Box::new(zed_actions::OpenProjectSettings),
        ),
        MenuEntry::Action(
            "Open Project Settings File",
            Box::new(zed_actions::OpenProjectSettingsFile),
        ),
        MenuEntry::Action(
            "Open Default Settings",
            Box::new(zed_actions::OpenDefaultSettings),
        ),
        MenuEntry::Separator,
        MenuEntry::Action("Open Keymap", Box::new(zed_actions::OpenKeymap)),
        MenuEntry::Action(
            "Open Keymap File",
            Box::new(zed_actions::OpenKeymapFile),
        ),
        MenuEntry::Action(
            "Open Default Key Bindings",
            Box::new(zed_actions::OpenDefaultKeymap),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Select Theme…",
            Box::new(zed_actions::theme_selector::Toggle::default()),
        ),
        MenuEntry::Action(
            "Select Icon Theme…",
            Box::new(zed_actions::icon_theme_selector::Toggle::default()),
        ),
    ]
}

fn file_menu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Action("New File", Box::new(workspace::NewFile)),
        MenuEntry::Separator,
        MenuEntry::Action("Open…", Box::new(workspace::Open::default())),
        MenuEntry::Action(
            "Open Recent…",
            Box::new(zed_actions::OpenRecent {
                create_new_window: Some(false),
            }),
        ),
        MenuEntry::Action(
            "Open Remote…",
            Box::new(zed_actions::OpenRemote {
                from_existing_connection: false,
                create_new_window: Some(false),
            }),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Add Folder to Project…",
            Box::new(workspace::AddFolderToProject),
        ),
        MenuEntry::Separator,
        MenuEntry::Action("Save", Box::new(workspace::Save { save_intent: None })),
        MenuEntry::Action("Save As…", Box::new(workspace::SaveAs)),
        MenuEntry::Action(
            "Save All",
            Box::new(workspace::SaveAll { save_intent: None }),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Close Editor",
            Box::new(workspace::CloseActiveItem::default()),
        ),
        MenuEntry::Action("Close Project", Box::new(workspace::CloseProject)),
    ]
}

fn edit_menu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Action("Undo", Box::new(editor::actions::Undo)),
        MenuEntry::Action("Redo", Box::new(editor::actions::Redo)),
        MenuEntry::Separator,
        MenuEntry::Action("Cut", Box::new(editor::actions::Cut)),
        MenuEntry::Action("Copy", Box::new(editor::actions::Copy)),
        MenuEntry::Action("Copy and Trim", Box::new(editor::actions::CopyAndTrim)),
        MenuEntry::Action("Paste", Box::new(editor::actions::Paste)),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Find in Buffer…",
            Box::new(zed_actions::buffer_search::Deploy::find()),
        ),
        MenuEntry::Action(
            "Find in Project…",
            Box::new(workspace::DeploySearch::default()),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Toggle Line Comment",
            Box::new(editor::actions::ToggleComments::default()),
        ),
    ]
}

fn selection_menu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Action("Select All", Box::new(editor::actions::SelectAll)),
        MenuEntry::Action(
            "Expand Selection",
            Box::new(editor::actions::SelectLargerSyntaxNode),
        ),
        MenuEntry::Action(
            "Shrink Selection",
            Box::new(editor::actions::SelectSmallerSyntaxNode),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Add Cursor Above",
            Box::new(editor::actions::AddSelectionAbove::default()),
        ),
        MenuEntry::Action(
            "Add Cursor Below",
            Box::new(editor::actions::AddSelectionBelow::default()),
        ),
        MenuEntry::Action(
            "Select Next Occurrence",
            Box::new(editor::actions::SelectNext {
                replace_newest: false,
            }),
        ),
        MenuEntry::Action(
            "Select Previous Occurrence",
            Box::new(editor::actions::SelectPrevious {
                replace_newest: false,
            }),
        ),
        MenuEntry::Action(
            "Select All Occurrences",
            Box::new(editor::actions::SelectAllMatches),
        ),
        MenuEntry::Separator,
        MenuEntry::Action("Move Line Up", Box::new(editor::actions::MoveLineUp)),
        MenuEntry::Action("Move Line Down", Box::new(editor::actions::MoveLineDown)),
        MenuEntry::Action(
            "Duplicate Selection",
            Box::new(editor::actions::DuplicateLineDown),
        ),
    ]
}

fn view_menu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Action(
            "Zoom In",
            Box::new(zed_actions::IncreaseBufferFontSize { persist: false }),
        ),
        MenuEntry::Action(
            "Zoom Out",
            Box::new(zed_actions::DecreaseBufferFontSize { persist: false }),
        ),
        MenuEntry::Action(
            "Reset Zoom",
            Box::new(zed_actions::ResetBufferFontSize { persist: false }),
        ),
        MenuEntry::Separator,
        MenuEntry::Action("Toggle Left Dock", Box::new(workspace::ToggleLeftDock)),
        MenuEntry::Action("Toggle Right Dock", Box::new(workspace::ToggleRightDock)),
        MenuEntry::Action(
            "Toggle Bottom Dock",
            Box::new(workspace::ToggleBottomDock),
        ),
        MenuEntry::Action("Toggle All Docks", Box::new(workspace::ToggleAllDocks)),
        MenuEntry::Submenu("Editor Layout", view_editor_layout_submenu_items),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Project Panel",
            Box::new(zed_actions::project_panel::ToggleFocus),
        ),
        MenuEntry::Action(
            "Outline Panel",
            Box::new(outline_panel::ToggleFocus),
        ),
        MenuEntry::Action(
            "Terminal Panel",
            Box::new(terminal_view::terminal_panel::ToggleFocus),
        ),
        MenuEntry::Separator,
        MenuEntry::Action("Diagnostics", Box::new(diagnostics::Deploy)),
    ]
}

fn view_editor_layout_submenu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Action(
            "Split Up",
            Box::new(workspace::SplitUp::default()),
        ),
        MenuEntry::Action(
            "Split Down",
            Box::new(workspace::SplitDown::default()),
        ),
        MenuEntry::Action(
            "Split Left",
            Box::new(workspace::SplitLeft::default()),
        ),
        MenuEntry::Action(
            "Split Right",
            Box::new(workspace::SplitRight::default()),
        ),
    ]
}

fn go_menu_items() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Action("Back", Box::new(workspace::GoBack)),
        MenuEntry::Action("Forward", Box::new(workspace::GoForward)),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Command Palette…",
            Box::new(zed_actions::command_palette::Toggle),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Go to File…",
            Box::new(workspace::ToggleFileFinder::default()),
        ),
        MenuEntry::Action(
            "Go to Symbol in Editor…",
            Box::new(zed_actions::outline::ToggleOutline),
        ),
        MenuEntry::Action(
            "Go to Line/Column…",
            Box::new(editor::actions::ToggleGoToLine),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Go to Definition",
            Box::new(editor::actions::GoToDefinition::default()),
        ),
        MenuEntry::Action(
            "Go to Declaration",
            Box::new(editor::actions::GoToDeclaration),
        ),
        MenuEntry::Action(
            "Go to Type Definition",
            Box::new(editor::actions::GoToTypeDefinition),
        ),
        MenuEntry::Action(
            "Find All References",
            Box::new(editor::actions::FindAllReferences::default()),
        ),
        MenuEntry::Separator,
        MenuEntry::Action(
            "Next Problem",
            Box::new(editor::actions::GoToDiagnostic::default()),
        ),
        MenuEntry::Action(
            "Previous Problem",
            Box::new(editor::actions::GoToPreviousDiagnostic::default()),
        ),
    ]
}
