//! Restarts a local language server that Android stopped.
//!
//! Language servers are the app's child processes, which Android kills under
//! memory pressure or when the app is cached, while upstream Zed only restarts
//! a server on request. Each local server's process is polled; when it is
//! gone and the server is still registered, the server is restarted, at most
//! [`RESTART_BUDGET`] times per name in [`RESTART_WINDOW`]. One more exit
//! stops it, as the status bar's Stop would, with a notification. See
//! `android-lsp-auto-restart.md`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use collections::HashSet;
use gpui::{App, AppContext as _, Entity, TaskExt as _};
use language::{LanguageServerId, LanguageServerName};
use lsp::LanguageServerSelector;
use project::{LspStore, LspStoreEvent, Project};

const LIVENESS_POLL: Duration = Duration::from_secs(3);
const RESTART_BUDGET: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(10 * 60);

type RecentExits = Rc<RefCell<HashMap<LanguageServerName, Vec<Instant>>>>;

pub fn init(cx: &mut App) {
    cx.observe_new(|project: &mut Project, _window, cx| {
        // A remote project's servers run on the remote host.
        if !project.is_local() {
            return;
        }
        let recent_exits = RecentExits::default();
        cx.subscribe(&project.lsp_store(), move |_, lsp_store, event, cx| {
            if let LspStoreEvent::LanguageServerAdded(server_id, _, _) = event {
                watch(lsp_store, *server_id, recent_exits.clone(), cx);
            }
        })
        .detach();
    })
    .detach();
}

fn watch(
    lsp_store: Entity<LspStore>,
    server_id: LanguageServerId,
    recent_exits: RecentExits,
    cx: &mut App,
) {
    let Some(server) = lsp_store.read(cx).language_server_for_id(server_id) else {
        return;
    };
    let Some(pid) = server.process_id() else {
        return;
    };
    let server = Arc::downgrade(&server);
    let lsp_store = lsp_store.downgrade();
    // The poll runs on a background thread so a live server never wakes the
    // main loop.
    let exited = cx.background_spawn({
        let executor = cx.background_executor().clone();
        async move {
            while process_is_running(pid) {
                executor.timer(LIVENESS_POLL).await;
            }
        }
    });
    cx.spawn(async move |cx| {
        exited.await;
        let Some(server) = server.upgrade() else {
            return;
        };
        lsp_store
            .update(cx, |lsp_store, cx| {
                on_exit(lsp_store, server_id, &server, &recent_exits, cx)
            })
            .ok();
    })
    .detach();
}

fn on_exit(
    lsp_store: &mut LspStore,
    server_id: LanguageServerId,
    server: &Arc<lsp::LanguageServer>,
    recent_exits: &RecentExits,
    cx: &mut gpui::Context<LspStore>,
) {
    // A server stopped or restarted through Zed is no longer registered
    // under this id, or is registered as its successor.
    match lsp_store.language_server_for_id(server_id) {
        Some(registered) if Arc::ptr_eq(&registered, server) => {}
        _ => return,
    }
    let buffers: Vec<_> = lsp_store
        .buffer_store()
        .read(cx)
        .buffers()
        .filter(|buffer| {
            lsp_store
                .language_server_ids_for_opened_buffer(buffer.read(cx).remote_id())
                .is_some_and(|servers| servers.contains(&server_id))
        })
        .collect();
    let name = server.name();
    if buffers.is_empty() {
        // A restart with no buffers only removes the server, which then
        // starts again when a buffer needs it; a stop would keep it stopped.
        log::info!("lsp_watchdog: {name} exited with no buffers open; removing it");
        lsp_store.restart_language_servers_for_buffers(
            Vec::new(),
            HashSet::from_iter([LanguageServerSelector::Id(server_id)]),
            cx,
        );
        return;
    }
    let selector = HashSet::from_iter([LanguageServerSelector::Name(name.clone())]);

    let now = Instant::now();
    let mut recent_exits = recent_exits.borrow_mut();
    let exits = recent_exits.entry(name.clone()).or_default();
    exits.retain(|exit| now.duration_since(*exit) < RESTART_WINDOW);
    let minutes = RESTART_WINDOW.as_secs() / 60;
    if exits.len() >= RESTART_BUDGET {
        log::warn!(
            "lsp_watchdog: {name} exited {RESTART_BUDGET} times within {minutes} minutes; \
             leaving it stopped"
        );
        lsp_store
            .stop_language_servers_for_buffers(buffers, selector, cx)
            .detach_and_log_err(cx);
        cx.emit(LspStoreEvent::Notification(format!(
            "{name} exited {RESTART_BUDGET} times in {minutes} minutes and stays stopped. If \
             Android stops it in the background, turn on Keep Awake in Settings → Android or \
             allow Zdroid's background activity in the battery settings, then restart the \
             server."
        )));
        return;
    }
    exits.push(now);
    let attempt = exits.len();
    log::warn!(
        "lsp_watchdog: {name} (pid {:?}) exited; restarting ({attempt}/{RESTART_BUDGET})",
        server.process_id()
    );
    lsp_store.restart_language_servers_for_buffers(buffers, selector, cx);
    cx.emit(LspStoreEvent::Notification(format!(
        "{name} exited; restarted ({attempt}/{RESTART_BUDGET})"
    )));
}

/// Whether `pid` is a live process: in `/proc` and not a zombie. The state
/// letter follows the parenthesized command name in `stat`.
fn process_is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(end_of_name) = stat.rfind(')') else {
        return false;
    };
    let state = stat[end_of_name + 1..].trim_start().bytes().next();
    !matches!(state, None | Some(b'Z') | Some(b'X'))
}
