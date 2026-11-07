# Storage permission JNI shim

**Status:** Active
**Phase / Commit:** runtime prompt since L2; All files access with the targetSdk 35 bump (2026-10-28)
**Files:** `MainActivity.kt` (`requestStoragePermissions`, `requestAllFilesAccess`), `crates/gpui_android/src/storage.rs`, `AndroidManifest.xml` (storage permissions), `crates/gpui_android/examples/zed_android/src/runtime_background.rs` (`AllowAllFilesAccess`), `crates/settings_ui/src/page_data.rs`

## Problem

Projects under the app files dir need no permission, but a folder picked
from external storage comes back as a raw `/storage/emulated/0/...` path
that `RealFs` reads directly, and the `~/storage` links point there too.
Without a grant those reads fail with `EACCES`.

## Constraint

The grant differs by Android version. Android 8 to 10: the runtime-prompted
`READ/WRITE_EXTERNAL_STORAGE` dialog. Android 11+ at targetSdk 30+: scoped
storage, where raw paths need `MANAGE_EXTERNAL_STORAGE` ("All files
access"), a per-app toggle in system settings that an app can only deep
link to, never prompt for.

## Solution

`MainActivity.requestStoragePermissions`, called from
`storage::request_once` at boot, checks
`Environment.isExternalStorageManager()` and opens the toggle screen once per
install (a shared preference remembers the ask). `requestAllFilesAccess` opens
the screen again from Settings > Android > Allow Access to /sdcard. The manifest declares `MANAGE_EXTERNAL_STORAGE`.

## Failure mode

`EACCES` on `/sdcard` paths with the toggle off: open Settings > Android >
Allow Access to /sdcard. Projects under `~/projects` work either way.

## See also

- `android-noexec-mount.md`: why builds cannot run from `/sdcard` even
  with access
