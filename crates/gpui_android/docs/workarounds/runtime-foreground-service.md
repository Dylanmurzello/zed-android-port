# Runtime foreground service and child reaping

**Status:** Active
**Phase / Commit:** rootfs engine, chunk 4a
**Files:** `crates/gpui_android/src/runtime_service.rs`, `crates/gpui_android/src/platform.rs` (tick and quit), `android/app/src/main/kotlin/com/zdroid/ZdRuntimeService.kt`, `MainActivity.kt` (`setRuntimeServiceActive`, battery exemption, `onDestroy`), `NativeBridge.kt` (`nativeBeforeProcessExit`), `AndroidManifest.xml`, `res/drawable/ic_runtime.xml`, `crates/settings_content/src/workspace.rs` (`android_runtime.keep_awake`), `crates/settings_ui/src/page_data.rs`, `crates/gpui_android/examples/zed_android/src/runtime_background.rs`

## Problem

Every tool the editor runs on Android is a child of the app process:
language servers, terminal shells, debug adapters, git, the tools those
start in turn. Android treats an app's children as phantom processes.
Once the app is cached (not visible, no foreground service) the phantom
process killer stops them, and the low-memory killer takes the app early.
A Rust project left indexing while the user reads a browser tab came back
to a dead rust-analyzer; a long build in the terminal died with the shell.
The chroot adapter never saw this: its children were forked by the root
daemon, outside the app's cgroup. The rootfs engine's children are ours.

Nothing reaped them either. Quit runs `std::process::exit`, which skips
destructors, and a task swiped away in Recents goes through
`MainActivity.onDestroy` straight to `Process.killProcess`. Children
survive both as orphans of init: an idle language server holding its RAM
until the next boot.

## Constraint

The only signal Android honors for "keep this app running" is a
foreground service with a notification. The system does not track an
app's native children as such, so the app itself has to know when it has
any. Hooking every spawn site is incomplete (PTY shells go through
alacritty's `openpty`, children start children, upstream adds spawn paths)
and tells nothing about exits, since `async-process` reaps children on
its own thread without a callback.

## Solution

Android mounts `/proc` with `hidepid=invisible`, so an app process sees
only its own uid: itself and its descendants. Measured in-app on the Tab
S9: three entries for Zed, the terminal's bash and a script. The kernel's
process table is therefore the registry of children, complete across
every spawn path and a handful of files to read.

`gpui_android::runtime_service` reads it from the platform loop every two
seconds (`/proc/<pid>/stat`, parent and state fields after the last `)`)
and walks the descendants of the editor's pid. Any live descendant means
the foreground service is wanted; the last one leaving starts a 30 second
debounce so a language server restart or a new terminal does not bounce
the notification. Transitions go through
`MainActivity.setRuntimeServiceActive(active, keepAwake)` to
`ZdRuntimeService`, a `Service` that calls `startForeground` with a
low-importance "Zdroid runtime is running" notification and, when
`android_runtime.keep_awake` is on, holds a partial wake lock so builds
continue with the screen off. `START_NOT_STICKY`: the Rust side restarts
the service from the process tree, a system restart with no editor
behind it would only show an orphan notification.

Reaping uses the same walk. `AndroidPlatform::quit` and
`MainActivity.onDestroy` (through `NativeBridge.nativeBeforeProcessExit`)
call `terminate_descendants`: SIGTERM to every live descendant, 200 ms of
grace polled every 20 ms, SIGKILL for the rest. Signals go to each process
rather than a group: the app itself sits in zygote's process group
(`pgid 1717` on the tablet, not its own pid), so a group signal on our
own pgid would hit zygote's group, and a terminal's shell owns a session
of its own. Zombies are skipped; they are reparented and reaped when the
process goes.

Battery: `MainActivity.requestIgnoreBatteryOptimizations` opens the
system exemption dialog (`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`,
falling back to the exemption list). The app asks once after the first
successful rootfs install (marker `files/etc/battery-exemption-asked`)
and the Android Runtime settings page has an "Allow background activity"
button for later.

## Why not the obvious fix

A pid registry at `util::command` plus `pre_exec(setpgid)` was the first
design. It misses the shells and the grandchildren, it still needs a poll
to notice exits, and process groups are the wrong unit here (see the
zygote pgid above). Reading the tree the kernel already keeps is both
shorter and complete.

A service that is always on would keep the notification up while the
editor idles with no children, which is the state the user sees most.

## Failure mode

The notification shows while a language server or terminal exists and
goes away 30 seconds after the last one exits. If it stays, something
under the app is still running: `ls /proc` from the Zed terminal lists
it. If children die in the background anyway, the device-wide phantom
process cap (32 on Android 12+) is the next suspect; it cannot be raised
from an app. Android 14+ exposes a Developer Options toggle, and over adb:

```sh
adb shell settings put global settings_enable_monitor_phantom_procs false
```

Samsung adds its own "Sleeping apps" list on top of the battery exemption;
an app put to sleep there loses its children regardless of the service.

`POST_NOTIFICATIONS` is runtime-granted on Android 13+. At targetSdk 28
the system asks on first use; if the user denies it the service still
runs, only the notification is hidden (the app then appears under Active
apps in the notification shade instead).

## See also

- `android-lsp-auto-restart.md` for what happens when a server dies anyway
- `targetsdk-28-execve.md`: the service does not depend on the pin, and
  `FOREGROUND_SERVICE_SPECIAL_USE` is already declared for a later bump
