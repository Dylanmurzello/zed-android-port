# Remote development on Android — env, channel, and CDN gotchas

**Status:** Active (verified on the rootfs engine on 2026-10-28 against a
macOS arm64 host with password auth through the static askpass helper: the
server processes on the host carried no `ZD_*`, `LD_PRELOAD`, or the
tablet's `HOME`/`TMPDIR`)
**Files:**
- `crates/zdroid_runtime/src/env.rs` (`ZED_BUILD_REMOTE_SERVER`)
- `crates/gpui_android/examples/zed_android/src/lib.rs` (`auto_update::init`)
- `crates/remote/src/transport/ssh.rs`
- `crates/auto_update/src/auto_update.rs`

## The one-line summary

Three things stand between an Android-built Zed and a working Open Remote
flow: the SSH transport's source-build fallback (which would need rustup,
zig and cargo-zigbuild on the tablet), the Dev release channel, under which
Zed refuses to download a remote server, and the auto-updater, which
resolves the server's download URL. ssh itself runs in the Ubuntu runtime,
and the local environment never crosses the tunnel.

## Symptom catalogue

### A. "rustup not found on $PATH, install rustup"

`build_remote_server_from_source` runs when Zed's CDN has no prebuilt
remote server matching the client version, and cross-compiles one with
`rustup`/`zig`/`cargo-zigbuild` on the **client**. Fix: the runtime sets
`ZED_BUILD_REMOTE_SERVER=never` in the app's environment, so the
source-build fallback returns `Ok(None)` and Zed falls through to the
CDN-or-existing-on-remote path.

### B. "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at …"

After (A), the SSH transport hits the `ReleaseChannel::Dev → bail!` branch:
the Dev channel refuses CDN downloads by design. The app stays on Dev, which
keeps its data dir and settings where they were and allows the newest
extension API. Fix: `ssh.rs` adds a `Dev if cfg!(target_os = "android")`
arm that asks for the server of the app's own version (`AppVersion`, the Zed
version in `crates/zed/Cargo.toml`), and names it by version like a stable
server, so a newer Zed never reuses an older server left on the host.
Client and server must match: newer clients start agents through server
subcommands older servers lack.

### C. `failed to fetch release: "invalid channel: dev"`

Zed's release CDN accepts only `stable`, `preview` and `nightly`. Fix:
`auto_update.rs::get_release_asset` asks the stable channel for the Dev
client's version and falls back to preview, since Zed never reuses a
version number across channels. This only works when the fork merges a
published Zed release tag.

### D. "auto-update not initialized"

The CDN download path needs `GlobalAutoUpdate` to resolve the asset URL.
Fix: `lib.rs` calls `auto_update::init` after `Client::production`, with
`ZED_UPDATE_EXPLANATION` set first so Zed's own update polling stays off:
Zed's updater can't install an APK, and `app_updater` checks this fork's
releases instead.

### E. The local environment on the remote

A remote terminal's environment is built from the remote's own and Zed's
`ZED_*` variables (`project/src/terminals.rs::create_remote_shell`), never
from the local process. The fork once added its Android variables inside
`terminal::insert_zed_terminal_env`, which also builds the remote's
environment, and the remote shell got the tablet's `HOME`, `PATH` and
`LD_PRELOAD`. Now the local pty inherits the app's environment, which
carries the runtime's variables (for a remote terminal that's the local
`ssh` process, which needs them), and `terminal.rs` and
`build_command_posix` are unchanged.

## Verifying on device

After Open Remote, in Zed's remote terminal:

```sh
# SHOULD print empty brackets:
echo "LD_PRELOAD=[$LD_PRELOAD]"
# SHOULD print the remote user's home, NOT /data/data/com.zdroid/...:
echo "HOME=$HOME"
env | grep '^ZD_'   # SHOULD print nothing
```

If something Android-specific shows up, check whether it comes from the
transport or from the remote's own shell startup:

```sh
ps -ef | grep zed-remote-server | grep -v grep
cat /proc/<remote_server_pid>/environ | tr '\0' '\n' | grep -E 'LD_PRELOAD|^ZD_|com.zdroid'
```

## Failure modes if regressed

- **(E)** Android variables added to `insert_zed_terminal_env` again: the
  tablet's `HOME`, `PATH` and the `ZD_*` engine variables reach the remote.
- **(B)** the Android arm lost in a merge: Open Remote bails on the Dev
  channel again.
- **(C)** the stable/preview funnel lost: "invalid channel: dev".
- **(D)** `auto_update::init` removed: every Open Remote attempt bails with
  "auto-update not initialized".
- **(A)** `ZED_BUILD_REMOTE_SERVER` unset: the client tries to cross-compile
  and fails on missing rustup/zig.

## Rootfs engine: what ssh needed from the loader

Three sandbox refusals sat between the password prompt and a working
ControlMaster, each fixed in the engine rather than in ssh (2026-10-28):

- ssh publishes its mux socket by binding a temporary name and hard-linking
  it to `ControlPath`; the sandbox refuses hard links and a socket cannot be
  copied, so the loader turns a refused `linkat` on a socket into a
  `renameat2(RENAME_NOREPLACE)`, the same atomic publish with the same
  `EEXIST` on a race (`crates/zd_loader/src/link.rs`).
- The master waits with `ppoll` and calls the legacy `accept`, which bionic
  never issues and the app seccomp policy therefore omits; the untagged SIGSYS
  used to come back as ENOSYS and the master spun on the readable listener.
  The compat table answers it with `accept4(fd, addr, len, 0)`
  (`crates/zd_engine_core/src/compat.rs`).
- The master checks the mux client's `getpeereid` against its own `getuid()`;
  under fake root `getuid()` is 0 while `SO_PEERCRED` reported the real app
  uid, so every client was dropped with "master hello exchange failed". The
  handler maps the peer credentials the way it maps stat owners
  (`crates/zd_loader/src/handler.rs::peer_credentials`).

The password prompt itself depended on a waker identity bug in async-task that
froze the main loop; see `async-task-waker-identity.md`.
