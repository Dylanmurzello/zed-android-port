# Copy projects from shared storage

**Status:** Active
**Files:** `examples/zed_android/src/noexec_modal.rs`, `src/storage.rs`, `src/project_import.rs` (under `crates/gpui_android`)

Android shared storage is mounted with `noexec`. The title-bar warning uses
`statvfs(ST_NOEXEC)` and offers to copy the project into the active runtime's
`~/projects` directory. It leaves the source project untouched. The modal uses
Zed's `AlertModal` and `StatusToast`, and opens the completed copy through
`MultiWorkspace::open_project` in the window that started the import.

The background importer reserves a destination with `create_dir`, appending
`-imported` or `-imported-N` on collisions. Reservation is atomic, so concurrent
imports cannot share a destination. Directory symlinks are copied as links,
regular files retain their permissions, and special files fail the import.
All I/O failures propagate to the dialog. Failure or cancellation removes the
reserved directory; a cleanup failure reports its path. Cancellation is checked
between entries, so copying a large individual file can delay it.

Closing the dialog or its window also cancels the worker. If a copy has already
finished before dismissal, its completed directory remains, but the late result
does not open a project. The toast uses the current workspace entity so it does
not try to borrow the click handler's window a second time.

Zed v1.26.2-pre's `fs::copy_recursive` follows directory symlinks and does not
provide cancellation. Keep the importer until that helper can satisfy both
requirements. The unused progress callback and best-effort skipping path were
removed in the 2026-10-28 review. Host tests cover symlink cycles, concurrent
imports, collisions, failure cleanup, cancellation and recursive destinations.

The dialog's suppression checkbox stores the original path in
`<workspace_root>/.cache/zed-android/noexec-suppressed.json`. Missing or invalid
suppression data shows the warning again. A failed `statvfs` check currently
omits the warning; passing that check alone does not guarantee that execution
will succeed (permissions, SELinux and binary compatibility also matter).

See [shared-storage execution limits](android-noexec-mount.md),
[the projects directory](projects-workspace-import.md), and
[trust restoration](trust-restore-from-db.md).
