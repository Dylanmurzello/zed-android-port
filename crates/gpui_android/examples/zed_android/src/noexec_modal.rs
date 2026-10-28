use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    AppContext as _, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, TaskExt as _, Window, rems,
};
use notifications::status_toast::StatusToast;
use theme::ActiveTheme;
use ui::{
    AlertModal, Button, ButtonCommon, ButtonStyle, Checkbox, Clickable, Color, Disableable,
    ElevationIndex, FluentBuilder as _, Headline, HeadlineSize, Icon, IconName, KeyBinding, Label,
    LabelCommon, ListBulletItem, ToggleState, h_flex, rems_from_px, v_flex,
};
use workspace::{DismissDecision, ModalView, MultiWorkspace};

pub struct NoexecMoveModal {
    abs_path: PathBuf,
    suppress: bool,
    focus_handle: FocusHandle,
    copy_cancel: Option<Arc<AtomicBool>>,
    error: Option<String>,
}

impl NoexecMoveModal {
    pub fn new(abs_path: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            abs_path,
            suppress: false,
            focus_handle: cx.focus_handle(),
            copy_cancel: None,
            error: None,
        }
    }

    fn copy_and_dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.copy_cancel.is_some() {
            return;
        }
        let Some(projects_root) = gpui_android::storage::projects_dir() else {
            self.error = Some("Configure an Android runtime before importing projects.".into());
            cx.notify();
            return;
        };
        let (Some(workspace_window), Some(multi_workspace)) = (
            window.window_handle().downcast::<MultiWorkspace>(),
            window.root::<MultiWorkspace>().flatten(),
        ) else {
            self.error = Some("Open a workspace before importing projects.".into());
            cx.notify();
            return;
        };
        let source = self.abs_path.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let toast = StatusToast::new("Copying project to ~/projects…", cx, {
            let cancel = cancel.clone();
            move |this, _cx| {
                this.icon(Icon::new(IconName::FolderOpen).color(Color::Info))
                    .auto_dismiss(false)
                    .action("Cancel", move |_, _| cancel.store(true, Ordering::Relaxed))
            }
        });
        let workspace = multi_workspace.read(cx).workspace().clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_status_toast(toast.clone(), cx);
        });
        self.copy_cancel = Some(cancel.clone());
        self.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let cancel = cancel.clone();
                    async move {
                        gpui_android::project_import::import_project(
                            &source,
                            &projects_root,
                            &cancel,
                        )
                    }
                })
                .await;
            toast.update(cx, |_, cx| cx.emit(DismissEvent));

            let result = match result {
                Ok(Some(destination))
                    if !cancel.load(Ordering::Relaxed) && this.upgrade().is_some() =>
                {
                    // Android can dismiss the dialog after the worker finishes
                    // but before this task resumes. Open only while it is active.
                    match workspace_window.update(cx, |workspace, window, cx| {
                        workspace.open_project(
                            vec![destination],
                            workspace::OpenMode::Activate,
                            window,
                            cx,
                        )
                    }) {
                        Ok(open) => open.await.map(|_| ()),
                        Err(error) => Err(error),
                    }
                }
                Ok(_) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = &result {
                log::error!("Project import failed: {error:#}");
            }
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.copy_cancel = None;
                    match result {
                        Ok(()) => cx.emit(DismissEvent),
                        Err(error) => {
                            this.error = Some(format!("Could not import project: {error:#}"));
                            cx.notify();
                        }
                    }
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn cancel_and_maybe_suppress(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.copy_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        if self.suppress {
            gpui_android::storage::add_noexec_suppressed(&self.abs_path);
        }
        cx.emit(DismissEvent);
    }
}

impl Focusable for NoexecMoveModal {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for NoexecMoveModal {}

impl Drop for NoexecMoveModal {
    fn drop(&mut self) {
        if let Some(cancel) = &self.copy_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl ModalView for NoexecMoveModal {
    fn fade_out_background(&self) -> bool {
        true
    }

    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        if let Some(cancel) = &self.copy_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        DismissDecision::Dismiss(true)
    }
}

impl Render for NoexecMoveModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path_label = self.abs_path.display().to_string();

        AlertModal::new("zed-android-noexec-modal")
            .width(rems(40.))
            .key_context("NoexecMoveModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.copy_and_dismiss(window, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, _window, cx| {
                this.cancel_and_maybe_suppress(cx);
            }))
            .header(
                v_flex()
                    .p_3()
                    .gap_1()
                    .rounded_t_md()
                    .bg(cx.theme().colors().editor_background.opacity(0.5))
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Warning).color(Color::Warning))
                            .child(
                                Headline::new("Builds won't run on shared storage")
                                    .size(HeadlineSize::Small),
                            ),
                    )
                    .child(
                        h_flex()
                            .pl_5()
                            .child(Label::new(path_label).color(Color::Muted)),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .child(
                                Label::new(
                                    "Android prevents running built programs from shared storage.",
                                )
                                .color(Color::Muted),
                            )
                            .child(
                                Label::new(
                                    "Copy this project to ~/projects to build and run it. The original stays where it is.",
                                )
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        v_flex()
                            .child(Label::new("Affected:").color(Color::Muted))
                            .child(ListBulletItem::new("cargo / rustc target dir builds"))
                            .child(ListBulletItem::new("go build / make / autotools"))
                            .child(ListBulletItem::new("npm rebuild / native node modules"))
                            .child(ListBulletItem::new("Any pip install --no-binary native wheel")),
                    )
                    .when_some(self.error.as_ref(), |this, error| {
                        this.child(Label::new(error.clone()).color(Color::Error))
                    })
                    .child(
                        Checkbox::new(
                            "noexec-suppress",
                            ToggleState::from(self.suppress),
                        )
                        .label("Don't warn me about this project again")
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                this.suppress = state.selected();
                                cx.notify();
                                cx.stop_propagation();
                            },
                        )),
                    ),
            )
            .footer(
                h_flex()
                    .px_3()
                    .pb_3()
                    .gap_1()
                    .justify_end()
                    .child(
                        Button::new("noexec-cancel", "Cancel")
                            .key_binding(
                                KeyBinding::for_action(&menu::Cancel, cx)
                                    .size(rems_from_px(12_f32)),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_and_maybe_suppress(cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        Button::new("noexec-copy", "Copy to ~/projects")
                            .disabled(self.copy_cancel.is_some())
                            .style(ButtonStyle::Filled)
                            .layer(ElevationIndex::ModalSurface)
                            .key_binding(
                                KeyBinding::for_action(&menu::Confirm, cx)
                                    .size(rems_from_px(12_f32)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.copy_and_dismiss(window, cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
    }
}
