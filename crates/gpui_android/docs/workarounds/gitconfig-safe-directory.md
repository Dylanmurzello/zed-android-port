# .gitconfig safe.directory = *

**Status:** Active
**Files:** `crates/zdroid_runtime/src/install.rs` (`seed`)

## Problem

git refuses to work in a repository it thinks belongs to another user
("detected dubious ownership"). In the runtime it always does: the engine
presents the guest as root, while the files belong to the app's uid, and a
repository under `/sdcard` belongs to `media_rw`.

## Solution

The installer seeds the rootfs's system-wide `/etc/gitconfig` with `[safe]
directory = *`. The runtime's git, which Zed and the terminal both run, reads
it before the user's own `~/.gitconfig`, which stays the user's.

## Failure mode

`git status` in the terminal, or Zed's Git panel, reports dubious ownership:
`/etc/gitconfig` lost the `[safe]` section. Reset Runtime seeds it again, and
`git config --global --add safe.directory '*'` works around it meanwhile.
