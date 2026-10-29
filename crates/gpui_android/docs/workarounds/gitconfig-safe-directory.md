# .gitconfig safe.directory = *

**Status:** Active
**Files:** `crates/zdroid_runtime/src/install.rs` (`seed`)

## Problem

git refuses to work in a repository it thinks belongs to another user
("detected dubious ownership"). In the runtime it always does: the engine
presents the guest as root, while the files belong to the app's uid, and a
repository under `/sdcard` belongs to `media_rw`.

## Solution

The installer seeds the guest's `/root/.gitconfig` with `[safe] directory =
*`. The runtime's git, which Zed and the terminal both run, reads it. A
Bootstrap-era home's own `.gitconfig` is included after it (`link_home`).

## Failure mode

`git status` in the terminal, or Zed's Git panel, reports dubious ownership:
`/root/.gitconfig` lost the `[safe]` section, for example after a user
replaced the file. `git config --global --add safe.directory '*'` restores it.
