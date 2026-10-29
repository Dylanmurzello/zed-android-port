# Storage Access Framework picker integration

**Status:** Active when `use_system_path_prompts` is `true`. Since 0.10.0
Zdroid defaults it to `false` (`lib.rs`), so Open, Open Files, Add Folder to
Project and Save As use Zed's own path prompts.
**Phase / Commit:** `0caf3f20b9` — Wire Android Storage Access Framework to gpui's path prompts
**Files:** `crates/gpui_android/src/saf.rs`, `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/.../MainActivity.kt`

## Why it's opt-in

Android's picker is for choosing documents another app owns. Zdroid's home
folder appears in it only as one more provider, in the side menu, and Zed can
only use what this bridge turns back into a path it could open anyway: a folder
on primary shared storage, which needs All files access, or one in Zdroid's own
home. Zed's built-in prompt starts in the home folder (`DirectoryLister::
default_query`, the editor's `HOME`) and reaches `/sdcard` by path or through
`~/storage/shared`, as desktop Zed's does with the same setting.

## Problem

gpui's `prompt_for_paths` and `prompt_for_new_path` traditionally pop a
native file dialog (NSOpenPanel on macOS, GTK FileChooser on Linux). On
Android there's no equivalent — the system path is the Storage Access
Framework (SAF), which fires `Intent`s like `ACTION_OPEN_DOCUMENT_TREE` and
returns `content://` URIs that don't map to filesystem paths directly.

## Constraints

Several Android quirks bite this flow:

1. **`ActivityResultLauncher` silently no-ops** when `launch()` is called
   from a non-STARTED lifecycle. The typical state when the call comes from
   gpui's render thread (which can be in any lifecycle state) is non-STARTED,
   so the modern API just doesn't fire. AGDK's own samples sidestep this by
   using the legacy `startActivityForResult` + `onActivityResult` path.
2. **`content://` URIs aren't filesystem paths.** RealFs, worktrees, project
   panel — every part of zed downstream of the picker assumes a POSIX path.
## Solution

Two pieces:

1. **JNI bridge** (`saf.rs`) holds a `Mutex<Option<Pending>>` slot. When
   gpui calls `prompt_for_paths`, we stash the oneshot sender and call
   `MainActivity.launchOpenTree()` via JNI.
2. **Legacy `startActivityForResult` path** in MainActivity.kt (not
   ActivityResultLauncher). Picked URI returned via
   `Java_dev_zed_zed_android_MainActivity_onPickerResult` JNI callback,
   which decodes to a POSIX path under `/storage/emulated/0` and resolves
   the stashed oneshot. Zed's own `prompt_for_open_path_and_open` then opens
   it, as on the desktop.

## Why this works

- Tree URIs from primary external storage have a predictable form
  (`content://com.android.externalstorage.documents/tree/primary%3A<percent-encoded-path>`)
  that decodes to a `/storage/emulated/0/<path>` POSIX path Android also
  exposes through normal filesystem syscalls.
- Legacy `startActivityForResult` works regardless of lifecycle state
  because it's a direct intent dispatch, not a state-machine-gated callback.

## Failure mode if regressed

- ActivityResultLauncher reintroduced → first SAF pick after backgrounding
  silently no-ops, no error UI, user is confused.
- Non-primary-volume URIs (e.g. SD card adopted storage with UUID volume
  IDs) hit the path decoder and silently fall through. Users see "couldn't
  find path" with a `content://` URI in the error. Mitigation documented;
  fix is to extend the decoder for non-primary volumes when needed.

## See also

- [projects-workspace-import.md](projects-workspace-import.md) — what
  happens after a path is picked
- [storage-permission-jni.md](storage-permission-jni.md) — runtime perms
  for direct RealFs reads after the SAF pick
