# Restricted Mode trust grants restored from WorkspaceDb at boot

**Status:** Active
**Phase / Commit:** L3 polish
**Files:** `crates/gpui_android/examples/zed_android/src/lib.rs` (around line 403)

## Problem

Every launch (and reinstall) showed the Restricted Mode trust prompt for
projects the user had previously trusted. Looked like data was being wiped.
It wasn't — the SQLite db at `<data>/files/db/0-dev/db.sqlite` was preserving
trust grants correctly across launches (verified mtime + row count) — we
just weren't loading them.

## Constraint

`project::trusted_worktrees::init(db_trusted_paths, cx)` takes the trust map
as a parameter. Production `crates/zed/src/main.rs:450-457` populates it
from `WorkspaceDb::global(cx).fetch_trusted_worktrees()`. Our
`examples/zed_android/src/lib.rs:403` was passing
`std::collections::HashMap::default()` — empty — so the in-memory trust
state was always fresh on every boot, even though the db on disk had rows.

## Solution

Mirror production's fetch:

```rust
let db_trusted_paths =
    match workspace::WorkspaceDb::global(cx).fetch_trusted_worktrees() {
        Ok(paths) => paths,
        Err(err) => {
            error!(
                "zed_android: fetch_trusted_worktrees failed at boot: \
                 {err:#} — starting with empty trust map"
            );
            std::collections::HashMap::default()
        }
    };
project::trusted_worktrees::init(db_trusted_paths, cx);
```

WorkspaceDb's `define_connection!` macro lazy-opens the connection on first
`global(cx)` call; AppDatabase being set as a global earlier in our boot
chain provides the slot. No init-order issue.

## Why this works

`fetch_trusted_worktrees` reads:

```sql
SELECT absolute_path, user_name, host_name FROM trusted_worktrees;
```

and folds rows into `HashMap<Option<RemoteHostLocation>, HashSet<PathBuf>>`.
For local trust grants (no remote host), the key is `None` and the set
contains the absolute path. `track_worktree_trust` consumes
`db_trusted_paths.get(&None)` when local worktrees are added, so the trust
state propagates correctly.

## Failure mode if regressed

- Pass `HashMap::default()` again → user re-prompted every launch. Easy to
  diagnose: check the boot log for absence of the no-error path.
- Open the project from a different absolute path than the trusted one →
  prompt fires. Trust is keyed by exact absolute path. Examples:
  - Trusted `/storage/emulated/0/projects/foo`, opened
    `/data/data/.../home/projects/foo` → different paths, trust doesn't
    transfer. Use the title-bar Move chip
    ([noexec-banner-move.md](noexec-banner-move.md)) to import + trust the
    local copy once.

## See also

- [noexec-banner-move.md](noexec-banner-move.md)
- [projects-workspace-import.md](projects-workspace-import.md)
