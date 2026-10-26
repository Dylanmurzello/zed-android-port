//! Logs go to logcat, as before, and to Zed's log file the way desktop Zed
//! writes it, so `zed: open log` works without adb.

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;

use editor::{Editor, MultiBuffer};
use gpui::{App, AppContext as _, Context, VisualContext as _, Window};
use language::Capability;
use log::Log;
use util::maybe;
use workspace::Workspace;
use workspace::notifications::{NotificationId, simple_message_notification::MessageNotification};

/// android-activity 0.6.1 logs "Spurious ALOOPER_POLL_CALLBACK from
/// ALooper_pollOnce() (ignored)" as an error every time the looper
/// dispatches our Choreographer callback: once per vsync, up to 120 lines a
/// second on a 120 Hz display. Nothing else useful comes out of that module.
/// Revisit if an android-activity upgrade starts reporting real input or
/// lifecycle errors there.
const SPURIOUS_VSYNC_LOG_MODULE: &str = "android_activity::activity_impl";

struct Logger {
    logcat: android_logger::AndroidLogger,
}

impl Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.logcat.enabled(metadata) || zlog::Zlog {}.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if record.target().starts_with(SPURIOUS_VSYNC_LOG_MODULE) {
            return;
        }
        self.logcat.log(record);
        zlog::Zlog {}.log(record);
    }

    fn flush(&self) {
        self.logcat.flush();
        zlog::flush();
    }
}

/// Logs to logcat from here on, and to the log file once [`init_log_file`]
/// has run. Logcat stays at info; the file follows Zed's `log` setting.
pub fn init_logger() {
    static LOGGER: OnceLock<Logger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| Logger {
        logcat: android_logger::AndroidLogger::new(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("zed_android"),
        ),
    });
    // `android_main` runs again when the activity is recreated, but the
    // process keeps the logger it set the first time.
    if log::set_logger(logger).is_ok() {
        log::set_max_level(log::LevelFilter::max());
        zlog::process_env(None);
        zlog::filter::refresh_from_settings(&HashMap::default());
    }
}

/// Opens `Zed.log`, which rotates into `Zed.log.old` at 1 MB, as on desktop.
/// Needs the custom data dir set and the logs dir created.
pub fn init_log_file() {
    static LOG_FILE: OnceLock<()> = OnceLock::new();
    LOG_FILE.get_or_init(|| {
        if let Err(error) = zlog::init_output_file(paths::log_file(), Some(paths::old_log_file())) {
            log::error!(
                "zed_android: could not open {}: {error}",
                paths::log_file().display()
            );
        }
    });
}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &workspace::OpenLog, cx| {
        workspace::with_active_or_new_workspace(cx, |workspace, window, cx| {
            open_log_file(workspace, window, cx);
        });
    });
}

// Ported from `open_log_file` in crates/zed/src/zed.rs, which the app doesn't
// link.
fn open_log_file(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    const MAX_LINES: usize = 1000;
    let app_state = workspace.app_state();
    let languages = app_state.languages.clone();
    let fs = app_state.fs.clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let log = {
            let result = futures::join!(
                fs.load(&paths::old_log_file()),
                fs.load(&paths::log_file()),
                languages.language_for_name("log")
            );
            match result {
                (Err(_), Err(e), _) => Err(e),
                (old_log, new_log, lang) => {
                    let mut lines = VecDeque::with_capacity(MAX_LINES);
                    for line in old_log
                        .iter()
                        .flat_map(|log| log.lines())
                        .chain(new_log.iter().flat_map(|log| log.lines()))
                    {
                        if lines.len() == MAX_LINES {
                            lines.pop_front();
                        }
                        lines.push_back(line);
                    }
                    Ok((
                        lines
                            .into_iter()
                            .flat_map(|line| [line, "\n"])
                            .collect::<String>(),
                        lang.ok(),
                    ))
                }
            }
        };

        let (log, log_language) = match log {
            Ok((log, log_language)) => (log, log_language),
            Err(e) => {
                struct OpenLogError;

                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_notification(
                            NotificationId::unique::<OpenLogError>(),
                            cx,
                            |cx| {
                                cx.new(|cx| {
                                    MessageNotification::new(
                                        format!(
                                            "Unable to access/open log file at path \
                                                    {}: {e:#}",
                                            paths::log_file().display()
                                        ),
                                        cx,
                                    )
                                })
                            },
                        );
                    })
                    .ok();
                return;
            }
        };
        maybe!(async move {
            let project = workspace
                .read_with(cx, |workspace, _| workspace.project().clone())
                .ok()?;
            let buffer = project
                .update(cx, |project, cx| {
                    project.create_buffer(log_language, false, cx)
                })
                .await
                .ok()?;
            buffer.update(cx, |buffer, cx| {
                buffer.set_capability(Capability::ReadOnly, cx);
                buffer.set_text(log, cx);
            });

            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title("Log".into()));

            let editor = cx
                .new_window_entity(|window, cx| {
                    let mut editor = Editor::for_multibuffer(buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor.set_breadcrumb_header(format!(
                        "Last {} lines in {}",
                        MAX_LINES,
                        paths::log_file().display()
                    ));
                    let last_multi_buffer_offset = editor.buffer().read(cx).len(cx);
                    editor.change_selections(Default::default(), window, cx, |s| {
                        s.select_ranges(Some(last_multi_buffer_offset..last_multi_buffer_offset));
                    });
                    editor
                })
                .ok()?;

            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
                })
                .ok()
        })
        .await;
    })
    .detach();
}
