# Bounded language server auto-restart

**Status:** Active
**Phase / Commit:** rootfs engine, chunk 4a
**Files:** `crates/gpui_android/examples/zed_android/src/lsp_watchdog.rs`

## Problem

Upstream Zed never restarts a language server on its own. A server that
dies stays listed as running until a request fails, and the only way back
is the manual "Restart Server" entry on the status bar's server button.
On a desktop a server dies from its own bugs, so that is the right
default. On Android a healthy server is killed from outside: the phantom
process killer, the low-memory killer, a battery policy. The foreground
service removes most of those, not all (the device-wide phantom cap, a
Samsung sleeping-apps entry, memory pressure).

## Constraint

The `Child` lives inside `LanguageServer`, is never waited on until
shutdown, and `async-process` reaps on its own thread without a hook, so
there is no exit event to subscribe to. The restart must be bounded: a
server that cannot run at all (bad binary, missing toolchain) would
otherwise restart forever, each time reindexing the project.

## Solution

Android only. When a server reaches `LanguageServerState::Running`,
`watch_language_server_process` spawns a task that reads
`/proc/<pid>/stat` every three seconds (the pid is the `Child`'s and, since
the loader execs the guest in place, the server's). A missing entry or a
zombie state means the process is gone; the task then calls
`on_exit`, which checks the server is still the
registered one (the same `Arc`, not a successor), collects the buffers
opened in it, and runs the same path as the manual action:
`restart_language_servers_for_buffers` with the server's name. The toast
reads "rust-analyzer exited; restarted (1/3)".

The budget is three restarts per server name in a sliding ten minute
window. The fourth death inside the window stops the server through the
normal stop path, marks the name stopped (so opening another buffer does
not start it again), and toasts the two hints: Keep Awake under Settings >
Android, or the battery exemption. The manual restart clears the
stopped mark as it always did.

A death with no buffers open in the server is not restarted, only
cleaned up: a restart with no buffers removes it, and it starts again
when a buffer needs it.

The watchdog lives in the app and uses only `LspStore`'s public API: it
watches the servers `LspStoreEvent::LanguageServerAdded` announces for
local projects, finds a server's buffers with
`language_server_ids_for_opened_buffer`, and restarts or stops it with
`restart_language_servers_for_buffers` and
`stop_language_servers_for_buffers`. The poll runs on a background
thread, so a live server never wakes the main loop.

## Why not the obvious fix

Polling `/proc` instead of reading stdout EOF: the EOF is seen inside
`lsp.rs` (`handle_incoming_messages`), an upstream file this fork does not
otherwise patch, and exposing it means a new callback on `LanguageServer`.
One `stat` read per server every three seconds costs nothing and keeps
Zed's crates unchanged.

## Failure mode

A server that keeps dying shows the budget toast within ten minutes and
stays stopped; restart it by hand from the status bar once the cause is
fixed. If restarts loop without a toast, the deaths are more than ten
minutes apart and the server really is being killed from outside: check
the runtime notification and the battery exemption.

## See also

- `runtime-foreground-service.md` for why servers die in the background
- Dylan's port keeps the same logic in `lsp_store.rs`; this fork moved it
  into the app so Zed merges don't touch it
