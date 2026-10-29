# targetSdk=28 for execve from /data/data

**Status:** Reverted
**Phase / Commit:** pin introduced for the bootstrap adapter; lifted with the rootfs engine (targetSdk 35, 2026-10-28)
**Files:** `android/app/build.gradle.kts` (`targetSdk`), `crates/gpui_android/src/selinux_canary.rs`

## Problem

At targetSdk 29+ an untrusted app may no longer `execve` files in its own
data dir. The bootstrap adapter exec'd a whole Termux-style userland from
there, so the app pinned targetSdk 28 to stay in the SELinux domain that
still allows it, at the price of the "built for an older version of
Android" install warning.

## Why it was reverted

The rootfs engine never execs an app-private file: the kernel only execs
the loader in the APK's native library dir, which every domain may run,
and the Ubuntu binaries are mapped into the loader's process. The pin
bought nothing, so targetSdk went to 35. What the level changed for the
app: raw `/sdcard` paths need All files access on Android 11+
(`storage-permission-jni.md`), the notification permission is a real
prompt (`runtime-foreground-service.md`), the foreground service type is
enforced (declared since chunk 4a).

## See also

- `selinux-canary.md`: the domain is logged at boot, no longer checked
- `rootfs-engine-integration.md`
