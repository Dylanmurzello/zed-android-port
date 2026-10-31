//! Vertical stack of `MenuBar` (top) and `TitleBar` (bottom). Mounts
//! into the workspace's single `titlebar_item` slot.
//!
//! On macOS, production renders only `title_bar` here because the
//! application menus (Zed | File | Edit | …) live in the system menu
//! bar OUTSIDE the app window. Android has no system menu bar, so we
//! own both rows: the application menu on top, the project / trust /
//! settings row below — same vertical order users see on Mac (system
//! menu bar above the app's title bar).

use gpui::{Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div};

use crate::menu_bar::MenuBar;
use crate::title_bar::TitleBar;

pub struct Header {
    menu_bar: Entity<MenuBar>,
    title_bar: Entity<TitleBar>,
}

impl Header {
    pub fn new(menu_bar: Entity<MenuBar>, title_bar: Entity<TitleBar>) -> Self {
        Self {
            menu_bar,
            title_bar,
        }
    }
}

impl Render for Header {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .w_full()
            .child(self.menu_bar.clone())
            .child(self.title_bar.clone())
    }
}
