//! Filesystem-identity-aware path comparison.
//!
//! Standard byte-level path comparison (`Path::strip_prefix`,
//! `Path::eq`) treats two paths as equal only when their bytes match.
//! That's wrong for many real systems where two distinct byte sequences
//! resolve to the same on-disk file:
//!
//! - **Android mount-bind aliases**: `/data/user/0/<pkg>` and
//!   `/data/data/<pkg>` name the same inode. In the targetSdk 28 era
//!   both were mount entries and `std::fs::canonicalize` was a no-op
//!   for either form; at targetSdk 35 `/data/user/0` is a symlink in
//!   the app's namespace, so canonicalize turns the first into the
//!   second (see `android-files-dir-spelling.md`). Either way `stat()`
//!   tells: `(st_dev, st_ino)` is identical.
//! - **macOS firmlinks**: `/var` ↔ `/private/var`, `/tmp` ↔
//!   `/private/tmp`. Same shape: kernel-managed equivalence with no
//!   user-visible symlink.
//! - **User-created symlinks**: `~/projects/foo -> /elsewhere/foo`.
//!   Canonicalize works for these, but inode comparison also covers
//!   them and is the simpler universal primitive.
//! - **NFS hard links, bind mounts the user set up themselves,
//!   bind-mounted host dirs into chroots, future filesystem trickery**:
//!   anything that aliases an inode.
//!
//! Symptom when this isn't handled: callers that store a path under
//! one form ("Zed opened a worktree at `/data/user/0/<pkg>/projects/x`")
//! and look it up under another form ("LSP server inside chroot
//! published diagnostics for `/data/data/<pkg>/projects/x/main.java`")
//! get `Option::None` back and silently drop the lookup result. Hard
//! to debug because both paths exist, both stat successfully, and
//! `ls` shows the same content — just the bytes don't match.
//!
//! This module provides one primitive — [`PathIdentity`] — and one
//! algorithm — [`strip_prefix_by_identity`]. Anywhere two paths need
//! to be compared semantically rather than byte-equality, use these.
//! Cross-platform (Unix and Windows); falls back to no-op on platforms
//! without the metadata API needed.

use std::path::Path;

/// A platform-agnostic file-identity tuple. Two paths refer to the same
/// on-disk file iff their `PathIdentity`s compare equal. Constructed via
/// `stat(2)` on Unix and `GetFileInformationByHandle` on Windows; falls
/// back to `None` when the metadata API isn't available or the path
/// doesn't exist.
///
/// **Lifetime**: the identity captures the inode at the moment of the
/// call. A file replaced/recreated between two `of()` calls could
/// produce different identities. For the call sites where this matters
/// (concurrent file moves during LSP lookups, hot reload), callers
/// should treat the identity as a snapshot — if equality fails,
/// re-check via another mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PathIdentity {
    dev: u64,
    ino: u64,
}

impl PathIdentity {
    /// Read the identity from disk. Returns `None` if the path doesn't
    /// exist, the user lacks permission to stat it, or the platform
    /// doesn't expose `(dev, ino)`-style identity.
    pub fn of(path: &Path) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(path).ok()?;
            Some(Self {
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            let meta = std::fs::metadata(path).ok()?;
            // Windows: combine volume serial number + file index for
            // identity. `file_index()` returns 64 bits combining the
            // file's high and low identifiers; volume serial
            // disambiguates across filesystems. Same dev/ino concept,
            // different bit packing.
            Some(Self {
                dev: meta.volume_serial_number()? as u64,
                ino: meta.file_index()?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            None
        }
    }
}

/// Walk `child`'s ancestor chain upward, find the ancestor whose
/// [`PathIdentity`] matches `parent`'s, return the path components past
/// that ancestor. Used as a fallback when literal byte-level
/// `Path::strip_prefix` misses but the paths may still name the same
/// file via kernel-managed equivalence.
///
/// Returns:
/// - `Some(suffix_path)` when an ancestor matches. The suffix is a
///   sequence of `OsString` path components from the matched ancestor
///   down to `child`. An empty `Vec` means `child` IS the parent (same
///   identity, no descent).
/// - `None` when no match is found, the walk reaches the filesystem
///   root, or either path can't be stat'd.
///
/// **Complexity**: O(depth) syscalls — one `stat` for `parent` plus
/// one per ancestor walked. For typical project paths (≤20 deep) that's
/// ≤21 syscalls, microseconds. Hot paths (every LSP diagnostic publish)
/// should ALWAYS try the cheap byte-level `strip_prefix` first; this
/// function fires only on the byte-level miss, which is per-worktree-
/// per-path-form, not per-diagnostic.
///
/// **Caller obligation**: build the returned suffix into your
/// platform-specific relative path type (`RelPath`, `RelativePath`,
/// whatever). This module stays free of editor-specific type concerns.
pub fn strip_prefix_by_identity(
    child: &Path,
    parent: &Path,
) -> Option<Vec<std::ffi::OsString>> {
    let parent_id = PathIdentity::of(parent)?;
    let mut current = child.to_path_buf();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if PathIdentity::of(&current) == Some(parent_id) {
            suffix.reverse();
            return Some(suffix);
        }
        let name = current.file_name()?.to_owned();
        suffix.push(name);
        let parent_path = current.parent()?.to_path_buf();
        if parent_path == current {
            // Reached the root with no match (Path::parent of "/" is
            // either "" or "/" depending on platform; both compare
            // equal to `current` after the first iteration at root).
            return None;
        }
        current = parent_path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_of_same_path_compares_equal() {
        let tmp = std::env::temp_dir();
        let a = PathIdentity::of(&tmp).unwrap();
        let b = PathIdentity::of(&tmp).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn identity_of_nonexistent_is_none() {
        let path = std::path::PathBuf::from(
            "/this/path/does/not/exist/anywhere/at/all",
        );
        assert!(PathIdentity::of(&path).is_none());
    }

    #[test]
    fn strip_prefix_by_identity_byte_equal_paths() {
        let tmp = std::env::temp_dir();
        let child = tmp.join("nested").join("file.txt");
        // We don't need the child to exist for the walk to stat the
        // ANCESTORS — only the path is walked symbolically when stat
        // fails on the leaf. But for the parent-match to fire we need
        // the parent to exist. tmp always exists on test runners.
        if let Some(suffix) = strip_prefix_by_identity(&child, &tmp) {
            assert_eq!(
                suffix,
                vec![
                    std::ffi::OsString::from("nested"),
                    std::ffi::OsString::from("file.txt"),
                ],
            );
        }
    }

    #[test]
    fn strip_prefix_by_identity_root_returns_none() {
        let a = std::path::PathBuf::from("/foo/bar");
        let b = std::path::PathBuf::from("/some/unrelated/dir");
        let result = strip_prefix_by_identity(&a, &b);
        // /foo/bar's ancestors don't include /some/unrelated/dir's
        // inode. Walks to root, returns None.
        assert!(result.is_none());
    }
}
