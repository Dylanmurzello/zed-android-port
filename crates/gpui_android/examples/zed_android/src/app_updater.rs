//! Desktop Zed's update experience for the APK. Desktop's `AutoUpdater`
//! can't be used here: it fetches Zed's own builds from zed.dev, installs
//! by replacing its files on disk, and doesn't poll on the Dev channel
//! Zdroid runs as. This mirrors its states, hourly polling, the
//! `auto_update` setting, the title bar's `UpdateButton` and the "Updated
//! to …" notification, on top of `gpui_android::updater`, which checks this
//! fork's GitHub releases and installs the APK through an Android install
//! session.
//!
//! One deliberate difference: an automatic check only offers the update,
//! and the ~170 MB download starts on click, because the device may be on
//! mobile data. A manual check downloads right away, as desktop does.

use std::path::PathBuf;
use std::time::Duration;

use db::kvp::KeyValueStore;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, DismissEvent, Entity, Global, PromptLevel, Task,
};
use gpui_android::updater::{self, InstallOutcome, UpdateCheck};
use log::{error, info};
use settings::SettingsStore;
use ui::{IconName, UpdateButton, prelude::*};
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UP_TO_DATE_DISPLAY_DURATION: Duration = Duration::from_secs(4);
const LAST_RUN_VERSION_KEY: &str = "zdroid-last-run-version";

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Idle,
    Checking,
    Available {
        version: String,
        download_urls: Vec<String>,
    },
    Downloading {
        version: String,
        progress: Option<f32>,
    },
    /// Downloaded, waiting for a click, e.g. after the user declined
    /// Android's install prompt.
    ReadyToInstall {
        version: String,
        apk_path: PathBuf,
    },
    /// In an install session, while Android asks the user to confirm.
    /// Installing kills the app, so this only ends on cancel or failure.
    Installing {
        version: String,
        apk_path: PathBuf,
    },
    Errored {
        error: String,
    },
}

pub struct AppUpdater {
    status: Status,
    manual: bool,
    dismissed: Option<Status>,
    showing_up_to_date: bool,
    pending: Option<Task<()>>,
    polling: Option<Task<()>>,
    up_to_date_timer: Option<Task<()>>,
}

struct GlobalAppUpdater(Entity<AppUpdater>);

impl Global for GlobalAppUpdater {}

pub fn init(cx: &mut App) {
    let updater = cx.new(|cx| {
        cx.observe_global::<SettingsStore>(|this: &mut AppUpdater, cx| this.update_polling(cx))
            .detach();
        let mut this = AppUpdater {
            status: Status::Idle,
            manual: false,
            dismissed: None,
            showing_up_to_date: false,
            pending: None,
            polling: None,
            up_to_date_timer: None,
        };
        this.update_polling(cx);
        this
    });
    cx.set_global(GlobalAppUpdater(updater));
    notify_if_app_was_updated(cx);
    // An APK only waits for the installer while the app runs, so any left
    // in the cache from an earlier run is stale.
    cx.background_spawn(async { updater::remove_downloaded_apks() })
        .detach_and_log_err(cx);
}

impl AppUpdater {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAppUpdater>()
            .map(|global| global.0.clone())
    }

    fn update_polling(&mut self, cx: &mut Context<Self>) {
        let enabled = cx
            .global::<SettingsStore>()
            .merged_settings()
            .auto_update
            .unwrap_or(true);
        if enabled == self.polling.is_some() {
            return;
        }
        self.polling = enabled.then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    if this.update(cx, |this, cx| this.check(false, cx)).is_err() {
                        break;
                    }
                    cx.background_executor().timer(POLL_INTERVAL).await;
                }
            })
        });
    }

    pub fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if manual {
            self.dismissed = None;
        }
        if self.pending.is_some() {
            self.manual |= manual;
            cx.notify();
            return;
        }
        self.manual = manual;
        self.showing_up_to_date = false;
        self.up_to_date_timer = None;
        let previous_status = self.status.clone();
        // Unlike desktop, keep an offered update on screen while an
        // automatic check runs, instead of hiding it until the check ends.
        if manual || matches!(self.status, Status::Idle | Status::Errored { .. }) {
            self.status = Status::Checking;
        }
        cx.notify();

        let check = cx.background_spawn(async { updater::check_for_update() });
        self.pending = Some(cx.spawn(async move |this, cx| {
            let result = check.await;
            this.update(cx, |this, cx| {
                this.pending = None;
                match result {
                    Ok(UpdateCheck::UpToDate { .. }) => {
                        this.status = Status::Idle;
                        if this.manual {
                            this.show_up_to_date(cx);
                        }
                    }
                    Ok(UpdateCheck::Available {
                        latest,
                        download_urls,
                        ..
                    }) => {
                        this.status = match previous_status {
                            Status::ReadyToInstall { version, apk_path } if version == latest => {
                                Status::ReadyToInstall { version, apk_path }
                            }
                            _ => Status::Available {
                                version: latest,
                                download_urls,
                            },
                        };
                        if this.manual {
                            this.download_or_install(cx);
                        }
                    }
                    Err(error) if this.manual => {
                        error!("zed_android: update check failed: {error:#}");
                        this.status = Status::Errored {
                            error: format!("{error:#}"),
                        };
                    }
                    // Stay quiet when an automatic check fails, e.g. while
                    // offline, and keep showing what an earlier check found.
                    Err(error) => {
                        info!("zed_android: automatic update check failed: {error:#}");
                        this.status = previous_status;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn download_or_install(&mut self, cx: &mut Context<Self>) {
        match self.status.clone() {
            Status::Available {
                version,
                download_urls,
            } => self.download(version, download_urls, cx),
            Status::ReadyToInstall { .. } => self.install(cx),
            _ => {}
        }
    }

    fn download(&mut self, version: String, download_urls: Vec<String>, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        // Ask for "Install unknown apps" before spending ~170 MB on an APK
        // Android wouldn't let us install.
        match updater::can_install_updates() {
            Ok(true) => {}
            Ok(false) => {
                self.request_install_permission(version, download_urls, cx);
                return;
            }
            Err(error) => {
                self.fail("checking the install permission", error, cx);
                return;
            }
        }
        self.dismissed = None;
        self.status = Status::Downloading {
            version: version.clone(),
            progress: None,
        };
        cx.notify();

        let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded();
        let download = cx.background_spawn({
            let version = version.clone();
            async move {
                updater::download_apk(&version, &download_urls, move |progress| {
                    progress_tx.unbounded_send(progress).ok();
                })
            }
        });
        self.pending = Some(cx.spawn(async move |this, cx| {
            // The channel closes when the download finishes and drops the sender.
            while let Some(progress) = progress_rx.next().await {
                this.update(cx, |this, cx| {
                    if let Status::Downloading {
                        progress: current, ..
                    } = &mut this.status
                    {
                        *current = progress;
                        cx.notify();
                    }
                })
                .ok();
            }
            let result = download.await;
            this.update(cx, |this, cx| {
                this.pending = None;
                match result {
                    Ok(apk_path) => {
                        this.status = Status::ReadyToInstall { version, apk_path };
                        this.install(cx);
                    }
                    Err(error) => {
                        error!("zed_android: update download failed: {error:#}");
                        this.status = Status::Errored {
                            error: format!("{error:#}"),
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn request_install_permission(
        &mut self,
        version: String,
        download_urls: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let permission = match updater::request_install_permission() {
            Ok(permission) => permission,
            Err(error) => {
                self.fail("opening the install permission settings", error, cx);
                return;
            }
        };
        self.pending = Some(cx.spawn(async move |this, cx| {
            let granted = permission.await.unwrap_or(false);
            this.update(cx, |this, cx| {
                this.pending = None;
                if granted {
                    this.download(version, download_urls, cx);
                } else {
                    info!("zed_android: update not downloaded, installing apps isn't allowed");
                }
            })
            .ok();
        }));
    }

    fn install(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let Status::ReadyToInstall { version, apk_path } = self.status.clone() else {
            return;
        };
        self.status = Status::Installing {
            version: version.clone(),
            apk_path: apk_path.clone(),
        };
        cx.notify();

        let session = cx.background_spawn({
            let apk_path = apk_path.clone();
            async move { updater::install_update(&apk_path) }
        });
        self.pending = Some(cx.spawn(async move |this, cx| {
            let outcome = match session.await {
                Ok(outcome) => outcome.await.ok(),
                Err(error) => Some(InstallOutcome::Failed(format!("{error:#}"))),
            };
            this.update(cx, |this, cx| {
                this.pending = None;
                match outcome {
                    Some(InstallOutcome::Failed(error)) => {
                        error!("zed_android: installing the update failed: {error}");
                        this.status = Status::Errored { error };
                    }
                    // Declined in Android's prompt: offer the install again.
                    Some(InstallOutcome::Cancelled) | None => {
                        this.status = Status::ReadyToInstall { version, apk_path };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn fail(&mut self, action: &str, error: anyhow::Error, cx: &mut Context<Self>) {
        error!("zed_android: {action} failed: {error:#}");
        self.status = Status::Errored {
            error: format!("{error:#}"),
        };
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.dismissed = Some(self.status.clone());
        cx.notify();
    }

    fn show_up_to_date(&mut self, cx: &mut Context<Self>) {
        self.showing_up_to_date = true;
        self.up_to_date_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(UP_TO_DATE_DISPLAY_DURATION)
                .await;
            this.update(cx, |this, cx| {
                this.showing_up_to_date = false;
                cx.notify();
            })
            .ok();
        }));
    }
}

/// The title bar's update button, matching desktop's `UpdateVersion`.
pub fn render_update_button(cx: &App) -> Option<AnyElement> {
    let updater = AppUpdater::global(cx)?;
    let this = updater.read(cx);
    if this.dismissed.as_ref() == Some(&this.status) {
        return None;
    }
    if this.showing_up_to_date {
        return Some(UpdateButton::up_to_date().into_any_element());
    }
    let dismiss = {
        let updater = updater.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            updater.update(cx, |this, cx| this.dismiss(cx));
        }
    };
    let button = match &this.status {
        Status::Checking if this.manual => {
            UpdateButton::new(IconName::LoadCircle, "Checking for Zdroid Updates…")
                .icon_animate(true)
                .disabled(true)
        }
        Status::Available { version, .. } => {
            let updater = updater.clone();
            UpdateButton::new(IconName::Download, "Update Available")
                .tooltip(UpdateButton::version_tooltip_message(version))
                .with_dismiss()
                .on_click(move |_, _, cx| {
                    updater.update(cx, |this, cx| this.download_or_install(cx));
                })
                .on_dismiss(dismiss)
        }
        Status::Downloading { version, progress } => {
            UpdateButton::new(IconName::Download, "Downloading Zdroid Update…")
                .progress(*progress)
                .tooltip(UpdateButton::downloading_tooltip_message(
                    version, *progress,
                ))
                .disabled(true)
        }
        Status::Installing { version, .. } => {
            UpdateButton::new(IconName::LoadCircle, "Installing Zdroid Update…")
                .icon_animate(true)
                .tooltip(UpdateButton::version_tooltip_message(version))
                .disabled(true)
        }
        Status::ReadyToInstall { version, .. } => {
            let updater = updater.clone();
            UpdateButton::new(IconName::Download, "Install Update")
                .tooltip(UpdateButton::version_tooltip_message(version))
                .with_dismiss()
                .on_click(move |_, _, cx| {
                    updater.update(cx, |this, cx| this.install(cx));
                })
                .on_dismiss(dismiss)
        }
        Status::Errored { error } => {
            // Desktop opens its log here. Zdroid logs to logcat, so show
            // the error in a prompt: a touch screen can't hover over the
            // tooltip.
            let error = error.clone();
            UpdateButton::errored(error.clone())
                .on_click(move |_, window, cx| {
                    drop(window.prompt(
                        PromptLevel::Warning,
                        "Failed to update Zdroid",
                        Some(&error),
                        &["OK"],
                        cx,
                    ));
                })
                .on_dismiss(dismiss)
        }
        Status::Idle | Status::Checking => return None,
    };
    Some(button.into_any_element())
}

/// Shows desktop's "Updated to …" notification on the first launch after
/// the app's version went up, however the new APK was installed.
fn notify_if_app_was_updated(cx: &mut App) {
    let current_version = updater::current_version();
    if current_version.is_empty() {
        return;
    }
    let kvp = KeyValueStore::global(cx);
    cx.spawn(async move |cx| {
        let last_run_version = kvp.read_kvp(LAST_RUN_VERSION_KEY)?;
        if last_run_version.as_deref() == Some(current_version.as_str()) {
            return anyhow::Ok(());
        }
        kvp.write_kvp(LAST_RUN_VERSION_KEY.to_string(), current_version.clone())
            .await?;
        if let Some(last_run_version) = last_run_version
            && updater::is_newer(&last_run_version, &current_version)
        {
            cx.update(|cx| show_update_notification(current_version, cx));
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn show_update_notification(version: String, cx: &mut App) {
    struct UpdateNotification;

    show_app_notification(
        NotificationId::unique::<UpdateNotification>(),
        cx,
        move |cx| {
            let release_notes_url = format!(
                "https://github.com/{}/releases/tag/v{version}",
                updater::RELEASE_REPO
            );
            cx.new(|cx| {
                MessageNotification::new(format!("Updated to Zdroid {version}"), cx)
                    .primary_message("View Release Notes")
                    .primary_on_click(move |_, cx| {
                        cx.open_url(&release_notes_url);
                        cx.emit(DismissEvent);
                    })
                    .show_suppress_button(false)
            })
        },
    );
}
