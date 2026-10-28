# Remote development on Android — env, channel, and CDN gotchas

**Status:** Active (working end-to-end against a Linux-x86_64 Vultr remote
from Tab S9 Ultra; Zed protocol, file tree, terminal, file ops all flow.
Re-verified 2026-10-28 on the rootfs engine against a macOS arm64 host with
password auth through the static askpass helper: the server processes on the
host carried no `ZD_*`, `LD_PRELOAD`, or the tablet's `HOME`/`TMPDIR`)
**Phase / Commit:** L10b; rootfs engine chunk 2 for the engine gaps below
**Files:**
- `crates/gpui_android/examples/zed_android/src/lib.rs`
- `crates/gpui_android/examples/zed_android/Cargo.toml`
- `crates/remote/src/transport/ssh.rs`
- `crates/auto_update/src/auto_update.rs`

## The one-line summary

Three things stand between an Android-built Zed and a working Open Remote
flow: the SSH transport's source-build fallback (which would need
rustup + zig + cargo-zigbuild on Termux), the auto-updater (uninitialized
on Android because we self-distribute via APK, but the remote_server
URL resolver depends on it), and Zed's worktree-shell env capture
(which forwards every Android-flavored env var across the SSH tunnel
and breaks the remote shell). All three are dealt with in lib.rs's
boot-time env setup + a small filter in `transport/ssh.rs::build_command_posix`.

## Symptom catalogue

### A. "rustup not found on $PATH, install rustup"

Path: `crates/remote/src/transport.rs:297-299`. Fires when
`build_remote_server_from_source` runs because Zed's CDN doesn't have a
prebuilt remote_server matching the client version. Requires
`rustup`/`zig`/`cargo-zigbuild` on the **client** to cross-compile a
remote_server for the target's triple. Fix: set
`ZED_BUILD_REMOTE_SERVER=never` so the source-build fallback returns
`Ok(None)` and Zed falls through to the CDN-or-existing-on-remote path.

### B. "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at …"

Path: `crates/remote/src/transport/ssh.rs:850-855`. After (A) is
disabled, the SSH transport hits the
`ReleaseChannel::Dev → bail!` branch — Dev channel hard-refuses CDN
download by design. **Fix (static-funnel patch):** add a `Dev if
cfg!(target_os = "android")` arm right above the `Dev` bail that
returns `Ok(None)` — same as Nightly. The Android port is always a
custom local build (channel branching has no meaning when there's
one ship target = the APK), so we treat Dev-on-Android the same as
Nightly for the URL-resolution path.

Earlier iterations of this doc set `ZED_RELEASE_CHANNEL=nightly` at
boot to flip the runtime channel and avoid the bail. That worked but
namespaced Zed's data dir per channel — settings.json / recent
projects / ssh_connections all looked "fresh" on first launch
because they'd been saved under the Dev-channel namespace. The
direct match-arm patch keeps the Dev-channel data dir, no rollover
maintenance, no env-var dance. Non-Android Dev clients keep the
original bail.

### C. "auto-update not initialized"

Path: `crates/auto_update/src/auto_update.rs:516`. Even with (A) + (B)
the CDN download path needs `GlobalAutoUpdate` initialized to resolve
the GitHub Releases URL for the remote_server asset. We don't normally
init `auto_update` on Android (distribution = user reinstalls APK).
Fix: call `auto_update::init(client.clone(), cx)` in `lib.rs` after
constructing `Client::production`, and pre-set
`ZED_UPDATE_EXPLANATION` so the polling subscription
(`auto_update.rs:248-262`) is suppressed — we don't want Zed
periodically self-updating the APK.

### D. `ld.so: object '/data/data/com.zdroid/files/usr/lib/libtermux-exec.so' from LD_PRELOAD cannot be preloaded: ignored.`

Sprayed on every command run via `remote_server` RPC. Caused by Zed's
worktree-shell env capture
(`crates/project/src/environment.rs::local_directory_environment`)
inheriting our entire Termux env from `profile.d/termux-exec.sh`, then
the SSH transport at
`crates/remote/src/transport/ssh.rs::build_command_posix`
forwarding it across the tunnel via
`exec env LD_PRELOAD=… HOME=… TERMUX__HOME=… ./remote_server`. Glibc
on the remote can't dlopen `/data/data/com.zdroid/...` (path
doesn't exist on Linux) and complains. **Bigger problem**: `HOME` and
`TMPDIR` were also being overridden, breaking remote `~/.ssh/`,
`~/.bashrc`, subprocess scratch space.

Fix: filter known-Android-only env vars in `build_command_posix`
before they reach the `exec env` invocation. Since the rootfs engine
replaced the Termux bootstrap, the captured environment is the guest's
(`HOME=/root`, `TMPDIR=/tmp`) plus the engine's own configuration, so
the list is: `LD_PRELOAD`, `HOME`, `TMPDIR` and anything starting with
`ZD_`. Generic dev env (NVM_DIR, PYENV_ROOT, RUSTUP_HOME, etc.) keeps
forwarding so non-Android Zed clients aren't regressed.

### F. `failed to fetch release: "invalid channel: dev"`

Path: `crates/auto_update/src/auto_update.rs:612` (CDN URL builder for
the remote_server asset). After (B) returns `Ok(None)` on Android-Dev,
the SSH transport falls through to the CDN download path. The URL is
built as `/releases/{release_channel.dev_name()}/{version}/asset` —
which interpolates to `/releases/dev/X.Y.Z/asset`. Zed's release CDN
only accepts `stable` / `preview` / `nightly` and returns the quoted
error for anything else. The `Dev` channel was never meant to hit the
CDN at all (gated by the bail in (B)) — our static-funnel patch
unblocks (B) but doesn't change the channel name plumbed into the URL.

Fix (second leg of the static funnel): substitute
`ReleaseChannel::Nightly.dev_name()` for `release_channel.dev_name()`
at the URL assembly site, only when `cfg!(target_os = "android")` and
`release_channel == ReleaseChannel::Dev`. Other call sites of
`dev_name()` in `auto_update.rs` (line 313 self-update URL, line 532
local-cache directory, line 986 telemetry) don't need the swap —
self-update polling is suppressed by `ZED_UPDATE_EXPLANATION`, the
local cache directory name is internal, and telemetry should record
the actual channel.

### E. (Pre-existing) Open Remote args needed for libtermux-auth's broken HOME

Apt-installed openssh has `/data/data/com.termux/files/home` baked
into `libtermux-auth.so`'s `getpwuid` shim. Zed's ssh subprocess
respects this baked path over `$HOME`, so without override args
ssh-keygen / known_hosts / id_ed25519 all resolve to the wrong
location. Workaround in `settings.json`:

```json
{
  "ssh_connections": [{
    "host": "<your.host>",
    "username": "<user>",
    "args": [
      "-o", "UserKnownHostsFile=/data/data/com.zdroid/files/home/.ssh/known_hosts",
      "-o", "IdentityFile=/data/data/com.zdroid/files/home/.ssh/id_ed25519",
      "-o", "StrictHostKeyChecking=accept-new"
    ]
  }]
}
```

Permanent fix is L10a — bake openssh into the bootstrap rebuild on
the Vultr instance with our package paths substituted at build time.
After that the args block can come out of settings.json.

## What the local app sets up (boot-time, in `lib.rs`)

```rust
std::env::set_var("ZED_BUILD_REMOTE_SERVER", "never");      // (A)
std::env::set_var(                                          // (C, suppress polling)
    "ZED_UPDATE_EXPLANATION",
    "Updates ship via the APK; reinstall to upgrade.",
);
std::env::remove_var("LD_PRELOAD");                         // partial (D), kept for our process
auto_update::init(client.clone(), cx);                      // (C, init for URL resolver)
```

The `LD_PRELOAD` removal isn't strictly required after the (D) filter
landed in `transport/ssh.rs`, but kept for hygiene — it ensures any
non-SSH subprocess we spawn (cargo invocations, language servers run
locally, etc.) doesn't see a path that's about to break on the next
shell startup re-source.

For (B), a separate static patch in `transport/ssh.rs` adds a
`Dev if cfg!(target_os = "android")` match arm above the existing
`Dev` bail; no env-var manipulation needed at boot. Channel stays at
whatever ReleaseChannel inferred, so the app data dir stays put.

## What the upstream Zed code change does

`crates/remote/src/transport/ssh.rs::build_command_posix` filters
input_env before assembling the `exec env K=V K=V ./remote_server`
invocation. The filter is keyed on env-var name and strips `LD_PRELOAD`,
`HOME`, `TMPDIR` and any `ZD_` prefix (the rootfs engine's
configuration block). Non-Android Zed clients are unaffected because
those env vars don't exist in their environment.

## Verifying the fix on device

After the build/install, in Zed's remote terminal (post-Open-Remote):

```sh
# SHOULD print empty brackets:
echo "LD_PRELOAD=[$LD_PRELOAD]"
# SHOULD print /root (or whatever the remote user's home is), NOT
# /data/data/com.zdroid/...:
echo "HOME=$HOME"
# SHOULD print /tmp or similar, NOT /data/data/.../usr/tmp:
echo "TMPDIR=${TMPDIR:-/tmp}"
# Smoke test: any command should run cleanly, no ld.so spam:
ls; pwd; whoami
```

If LD_PRELOAD shows up despite the filter, run:

```sh
ps -ef | grep zed-remote-server | grep -v grep
cat /proc/<remote_server_pid>/environ | tr '\0' '\n' | grep -E 'LD_PRELOAD|TERMUX'
```

That tells you whether it's leaking from the transport's `exec env`
line (filter regressed) or from the remote's own bash startup
(separate problem — check `~/.bashrc`, `/etc/profile.d/`).

## Failure modes if regressed

- **(D) filter list too narrow**: a new Termux env var leaks across.
  Add it to the filter at `transport/ssh.rs::build_command_posix`.
  Logcat won't show this; check the remote shell's `cat /proc/$$/environ`.
- **(D) filter list too wide**: a legit user env var (NVM, etc.) gets
  stripped. Symptom: that toolchain doesn't work on remote. Check the
  filter; only Android-only names should be in it.
- **(B) channel switch undone**: Open Remote starts hard-bailing again
  on Dev channel. settings.json from Dev-channel state will look
  empty; original is on disk under the Dev-channel data dir
  (different namespace).
- **(C) auto_update::init removed**: every Open Remote attempt bails
  with "auto-update not initialized". Re-add the call in lib.rs.
- **(A) ZED_BUILD_REMOTE_SERVER unset**: client tries to cross-compile,
  fails on missing rustup/zig. Re-set the var in lib.rs.

## See also

- [phase-l10-remote-development-scope.md](../phase-l10-remote-development-scope.md)
  — original scope doc; L10a (bootstrap rebuild for openssh) is the
  permanent fix for symptom (E)
- [termux-bootstrap-rebuild.md](https://github.com/Dylanmurzello/zdroid-bootstrap/blob/main/docs/termux-bootstrap-rebuild.md) — the
  bootstrap-rebuild process L10a feeds into

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
