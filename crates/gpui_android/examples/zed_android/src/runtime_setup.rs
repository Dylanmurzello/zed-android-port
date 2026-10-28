//! Setup of the Ubuntu runtime without a step for the user: the first
//! launch extracts the bundled image before the editor shows, a failure
//! lands in a notification with Retry, and the Android Runtime settings
//! page offers a reset and the essentials step (git and CA certificates,
//! which the base image does not ship).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Context, DismissEvent, PromptLevel, Window, actions};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};
use zdroid_runtime::progress::LogProgressSink;
use zdroid_runtime::{ProgressSink, Runtime, install};

actions!(
    zdroid_runtime,
    [
        /// Extract the bundled Ubuntu image again, replacing everything
        /// installed inside the runtime.
        ResetRuntime,
        /// Install git and CA certificates into the runtime with apt.
        InstallEssentials,
    ]
);

/// Files earlier releases left under the data dir that nothing can use
/// anymore: the bootstrap prefix (`usr`, up to 240 MB of a userland only
/// the removed adapter could run), the old `bin/zd-exec` bridge binary and
/// the adapter selection file. User content (`home`, the editor's own
/// state under it) is never touched.
pub fn sweep_previous_runtimes(data_path: &Path) {
    for relative in ["bin/zd-exec", "etc/zd-runtime.toml"] {
        let path = data_path.join(relative);
        match std::fs::remove_file(&path) {
            Ok(()) => log::info!("runtime_setup: removed {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("runtime_setup: cannot remove {}: {error}", path.display()),
        }
    }
    let prefix = data_path.join("usr");
    if !prefix.is_dir() {
        return;
    }
    // Renamed first so a launch cut short mid-deletion leaves a name no
    // future code looks for; deleting a package tree takes seconds and
    // the editor need not wait for it.
    let doomed = data_path.join("usr.removing");
    let target = match std::fs::rename(&prefix, &doomed) {
        Ok(()) => doomed,
        Err(error) => {
            log::warn!("runtime_setup: cannot rename {}: {error}", prefix.display());
            prefix
        }
    };
    std::thread::Builder::new()
        .name("sweep-prefix".into())
        .spawn(move || match std::fs::remove_dir_all(&target) {
            Ok(()) => log::info!(
                "runtime_setup: removed the old bootstrap prefix {}",
                target.display()
            ),
            Err(error) => log::warn!("runtime_setup: cannot remove {}: {error}", target.display()),
        })
        .log_err();
}

/// What the boot-time setup did.
#[derive(Debug, Clone)]
pub enum BootSetup {
    AlreadyInstalled,
    Installed,
    Failed(String),
}

/// Extract the bundled image when no rootfs is installed. Runs before
/// the editor shows; the splash covers the seconds it takes.
pub fn setup_at_boot(app: &AndroidApp, runtime: &Runtime) -> BootSetup {
    if runtime.is_installed() {
        return BootSetup::AlreadyInstalled;
    }
    let started = Instant::now();
    let result = gpui_android::runtime_links::bundled_rootfs_image(app).and_then(|mut image| {
        install::install(
            runtime,
            &mut image,
            &zdroid_runtime::nameservers(),
            &mut LogProgressSink,
        )
    });
    match result {
        Ok(()) => {
            log::info!(
                "runtime_setup: rootfs installed from the bundled image in {:.1} s",
                started.elapsed().as_secs_f64()
            );
            BootSetup::Installed
        }
        Err(error) => {
            log::error!("runtime_setup: rootfs setup failed: {error:#}");
            BootSetup::Failed(format!("{error:#}"))
        }
    }
}

pub fn register(boot: BootSetup, cx: &mut App) {
    cx.observe_new(
        move |workspace: &mut Workspace, _window, cx: &mut Context<Workspace>| {
            workspace.register_action(reset_runtime);
            workspace.register_action(install_essentials);
            static BOOT_HANDLED: AtomicBool = AtomicBool::new(false);
            if BOOT_HANDLED.swap(true, Ordering::AcqRel) {
                return;
            }
            match &boot {
                BootSetup::AlreadyInstalled => {}
                BootSetup::Installed => {
                    crate::runtime_background::prompt_battery_exemption_once();
                    start_job(workspace, Job::Essentials, cx);
                }
                BootSetup::Failed(reason) => show_failure(workspace, reason, cx),
            }
        },
    )
    .detach();
}

fn reset_runtime(
    workspace: &mut Workspace,
    _: &ResetRuntime,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let runtime = Runtime::new(&zdroid_runtime::data_path());
    if !runtime.is_installed() {
        start_job(workspace, Job::Install, cx);
        return;
    }
    let answer = window.prompt(
        PromptLevel::Warning,
        "Reset the Ubuntu runtime?",
        Some(
            "Everything installed inside it with apt, cargo, pip or npm is removed and the \
             base image is extracted again. Projects under ~/projects and the editor \
             settings are kept. Running terminals and language servers are stopped.",
        ),
        &["Reset", "Cancel"],
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        if answer.await != Ok(0) {
            return;
        }
        workspace
            .update_in(cx, |workspace, _window, cx| {
                start_job(workspace, Job::Install, cx)
            })
            .ok();
    })
    .detach();
}

fn install_essentials(
    workspace: &mut Workspace,
    _: &InstallEssentials,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    start_job(workspace, Job::Essentials, cx);
}

fn show_failure(workspace: &mut Workspace, reason: &str, cx: &mut Context<Workspace>) {
    let message = format!(
        "The Ubuntu runtime could not be set up: {reason}. The terminal and language \
         servers need it."
    );
    workspace.show_notification(notification_id(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(message.clone(), cx)
                .primary_message("Retry")
                .primary_on_click(|window, cx| {
                    match cx.build_action("zdroid_runtime::ResetRuntime", None) {
                        Ok(action) => window.dispatch_action(action, cx),
                        Err(error) => log::warn!(
                            "runtime_setup: zdroid_runtime::ResetRuntime not registered: {error}"
                        ),
                    }
                    cx.emit(DismissEvent);
                })
        })
    });
}

#[derive(Clone, Copy, Debug)]
enum Job {
    Install,
    Essentials,
}

impl Job {
    fn title(self) -> &'static str {
        match self {
            Self::Install => "Setting up the Ubuntu runtime",
            Self::Essentials => "Installing git and certificates",
        }
    }

    fn done_message(self) -> &'static str {
        match self {
            Self::Install => {
                "Ubuntu runtime reset. Restart Zdroid to reload the terminal and language servers."
            }
            Self::Essentials => "git and CA certificates are installed.",
        }
    }

    fn run(self, progress: &mut dyn ProgressSink) -> Result<()> {
        let runtime = Runtime::new(&zdroid_runtime::data_path());
        match self {
            Self::Install => {
                let app = gpui_android::android_app().context("AndroidApp not registered")?;
                let mut image = gpui_android::runtime_links::bundled_rootfs_image(app)?;
                // Open descriptors keep the renamed tree alive, so a shell
                // or server left running would keep using the old rootfs
                // while new spawns land in the new one.
                gpui_android::runtime_service::terminate_descendants();
                install::install(
                    &runtime,
                    &mut image,
                    &zdroid_runtime::nameservers(),
                    progress,
                )
            }
            Self::Essentials => install::install_essentials(&runtime, progress),
        }
    }
}

enum JobEvent {
    Step(String),
    Warn(String),
    Finished(Result<(), String>),
}

struct ChannelSink {
    sender: Sender<JobEvent>,
}

impl ProgressSink for ChannelSink {
    fn step(&mut self, label: &str) {
        log::info!("runtime_setup: {label}");
        self.sender.send(JobEvent::Step(label.to_string())).ok();
    }

    fn progress(&mut self, _done: u64, _total: u64) {}

    fn warn(&mut self, message: &str) {
        log::warn!("runtime_setup: {message}");
        self.sender.send(JobEvent::Warn(message.to_string())).ok();
    }
}

static JOB_RUNNING: AtomicBool = AtomicBool::new(false);

/// Run `job` on its own thread and mirror its steps into one workspace
/// notification until it finishes. One job at a time: a reset while
/// apt runs would pull the tree from under it.
fn start_job(workspace: &mut Workspace, job: Job, cx: &mut Context<Workspace>) {
    if JOB_RUNNING.swap(true, Ordering::AcqRel) {
        log::info!("runtime_setup: a runtime job is already running; ignoring {job:?}");
        return;
    }
    let (sender, receiver) = mpsc::channel::<JobEvent>();
    std::thread::Builder::new()
        .name("runtime-setup".into())
        .spawn(move || {
            let mut sink = ChannelSink {
                sender: sender.clone(),
            };
            let result = job.run(&mut sink).map_err(|error| format!("{error:#}"));
            sender.send(JobEvent::Finished(result)).ok();
        })
        .log_err();
    show_progress(workspace, job.title().to_string(), cx);
    cx.spawn(async move |workspace, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let finished = workspace
                .update(cx, |workspace, cx| drain(workspace, job, &receiver, cx))
                .unwrap_or(true);
            if finished {
                break;
            }
        }
        JOB_RUNNING.store(false, Ordering::Release);
    })
    .detach();
}

/// Apply queued events; returns true once the job thread is done.
fn drain(
    workspace: &mut Workspace,
    job: Job,
    receiver: &Receiver<JobEvent>,
    cx: &mut Context<Workspace>,
) -> bool {
    let mut latest_step = None;
    while let Ok(event) = receiver.try_recv() {
        match event {
            JobEvent::Step(label) => latest_step = Some(label),
            JobEvent::Warn(message) => latest_step = Some(message),
            JobEvent::Finished(result) => {
                let message = match result {
                    Ok(()) => job.done_message().to_string(),
                    Err(error) => format!("{} failed: {error}", job.title()),
                };
                log::info!("runtime_setup: {message}");
                workspace.show_notification(notification_id(), cx, |cx| {
                    cx.new(|cx| MessageNotification::new(message.clone(), cx))
                });
                return true;
            }
        }
    }
    if let Some(step) = latest_step {
        show_progress(workspace, format!("{}: {step}", job.title()), cx);
    }
    false
}

fn show_progress(workspace: &mut Workspace, text: String, cx: &mut Context<Workspace>) {
    workspace.show_notification(notification_id(), cx, |cx| {
        cx.new(|cx| MessageNotification::new(text.clone(), cx))
    });
}

fn notification_id() -> NotificationId {
    struct RuntimeSetupNotification;
    NotificationId::unique::<RuntimeSetupNotification>()
}
