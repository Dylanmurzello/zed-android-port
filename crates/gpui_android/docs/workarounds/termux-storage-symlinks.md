# ~/storage curated symlinks

**Status:** Active
**Files:** `crates/gpui_android/src/storage.rs` (`setup_user_symlinks`), `crates/zdroid_runtime/src/install.rs` (`link_home`)

At every launch (idempotent), `gpui_android::storage::setup_user_symlinks`
creates `storage/{shared,downloads,dcim,documents,movies,music,pictures,podcasts}`
in the app's home folder (`<data>/files/home`), pointing at the
corresponding `/storage/emulated/0/<dir>`, plus `storage/external-N` for SD
cards and OTG drives, as Termux's `termux-setup-storage` does. The runtime's
home links to the folder (`/root/storage`), so `cd ~/storage/downloads`
works in the terminal.

`/storage/emulated/0` is FUSE-mounted noexec, so these are for opening and
editing files, never a workspace root; see
[android-noexec-mount.md](android-noexec-mount.md).
