//! `MenuBar` above `TitleBar`, in the workspace's single `titlebar_item` slot.

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
