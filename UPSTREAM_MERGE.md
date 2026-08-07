# Upstream merge policy

This repo is a fork of [zed-industries/zed](https://github.com/zed-industries/zed) carrying the Android port (Zdroid). We periodically pull upstream changes; this doc describes the policy.

## Remotes

```sh
git remote -v
# origin   git@github.com:Dylanmurzello/zed-android-port.git  (us)
# upstream https://github.com/zed-industries/zed              (Zed)
```

## Files we own, always

Identity + Zdroid-specific docs that we have actually authored. Conflicts on these always resolve to OURS via `.gitattributes` (see below). Verified via `git diff --quiet $BASE origin/main -- <file>` (BASE = `git merge-base origin/main upstream/main`):

- `README.md` (we rewrote upstream's)
- `CONTRIBUTING.md` (we rewrote)
- `BACKLOG.md`, `RELEASING.md`, `UPSTREAM_MERGE.md` (new files, ours)
- `.github/ISSUE_TEMPLATE/*` (we rewrote upstream's)
- `.github/pull_request_template.md` (we rewrote)
- `.github/FUNDING.yml` (we repointed from zed-industries to Dylanmurzello)
- `.gitattributes` (we extended)

Plus the two crates that don't exist upstream (entirely ours):

- `crates/gpui_android/**`
- `crates/zdroid_runtime/**`

**NOT in this list** (verified upstream-unchanged, so future upstream improvements should flow through):

- `AGENTS.md`, `GEMINI.md`, `CLAUDE.md` (upstream Zed's agent-rule files)
- `.git-blame-ignore-revs`, `.mailmap`, `.prettierrc`, `.rules`, `clippy.toml`, `rustfmt.toml`, `typos.toml` (upstream's style configs)
- `LICENSE-AGPL`, `LICENSE-APACHE`, `LICENSE-GPL` (inherited licenses)
- `crates/gpui_platform/`, `crates/onboarding/` (upstream crates we modified in-place; normal 3-way merge applies)

## Files we modify in-place

The upstream Zed files where we add cfg-gates, env-aware paths, or other small patches (including `crates/gpui_platform/` and `crates/onboarding/`). The count drifts between merges (2026-08 measured ~80 modified files); enumerate the current set rather than trusting a number:

```sh
git diff --name-only $(git merge-base origin/main upstream/main)..origin/main \
  | grep -vE "^(crates/gpui_android|crates/zdroid_runtime)/"
```

These use normal 3-way merge. Expect occasional conflicts when upstream refactors them; resolve case-by-case.

### Helpers the fork extracted (silent-staleness class)

Some functions were MOVED out of `crates/zed` so the Android entry point can reach them without linking the desktop binary crate: `editor::open_local_file`, `editor::open_bundled_file`, `editor::init_bundled_file_actions`, `workspace::open_settings_file`, `workspace::init_settings_file_actions`. Upstream keeps evolving its originals inside `crates/zed`, and because our copies live at different paths the merge produces NO conflict there — the extracted copies silently freeze. On every merge, diff each extracted helper against upstream's version of the original and port body/signature changes by hand (2026-08: `open_local_file` gained a `Option<Task<Result<Entity<Editor>>>>` return; `open_settings_file` was made sequential).

### Action namespace rule

`actions!(zed, [...])` is declared in BOTH `crates/zed/src/zed.rs` and `crates/zed_actions`. One home per action name, ever: an action referenced outside `crates/zed` (workspace/editor registrations, the Android app) must live in `zed_actions`, and `zed.rs` must not re-declare it. Keep-both compiles and then breaks action dispatch at runtime (the registry keys on name strings).

When you resolve a conflict in one of these, ALSO check if a workaround doc under `crates/gpui_android/docs/workarounds/` covers the affected file: the doc explains why the patch is there, which makes the resolution obvious.

## Merge driver setup (one-time, per clone)

`.gitattributes` declares `merge=ours` for owned files. Git needs the `ours` driver registered in your local config:

```sh
git config merge.ours.driver true
```

Without that, `merge=ours` is silently ignored and conflicts on identity files re-appear. Verify with:

```sh
git config --get merge.ours.driver  # should print: true
```

## Performing an upstream merge

```sh
git fetch upstream main
git checkout -b upstream-sync-$(date +%Y%m%d)
git merge upstream/main
# expected:
#   - automatic merges on most files
#   - automatic ours-merge on identity files (no prompt)
#   - modify/delete conflicts on upstream CI/legal/nix we deleted: `git rm` them in bulk.
#     ALSO sweep for files upstream ADDED under .github/workflows, .cloudflare/, nix/,
#     legal/ since the merge-base — they merge cleanly as additions and must be removed
#     by hand (merge=ours can't help; git never invokes merge drivers for adds/deletes).
#   - 3-way conflicts on the modified upstream files, resolve manually

cargo check --workspace          # gate 1: desktop
cd crates/gpui_android/examples/zed_android && \
  ANDROID_NDK_HOME=<ndk> RUSTFLAGS="-C target-feature=+fp16" \
  cargo ndk --platform 26 -t arm64-v8a check   # gate 2: THE gate
```

**Treat the `cargo ndk check` as a hard gate, not polish.** Everything behind
`cfg(target_os = "android")` and the whole example workspace is invisible to
`cargo check --workspace`; in both the 2026-05 and 2026-08 merges, every
Android break (orphaned cfg blocks, upstream API drift in fork UI code,
removed trait methods) surfaced only here.

Example-workspace rules while resolving:

- `crates/gpui_android/examples/zed_android/Cargo.toml` mirrors two things by
  hand: the ~90 path deps (upstream crate deletions/renames surface as
  manifest-load errors, not merge conflicts — e.g. `git_graph` in 2026-08),
  and root's entire `[patch.crates-io]` table (cargo only honors `[patch]`
  from the current workspace root; an unmirrored fork pin resolves from
  crates.io and only works by luck).
- Regenerate the example `Cargo.lock` via the ndk check and COMMIT IT ON THE
  SYNC BRANCH, so the branch is self-consistent before it reaches main.
- Never sync away the example's `[profile]` overrides or
  `.cargo/config.toml` (`+fp16`).

Then land and verify on hardware:

```sh
git checkout main && git merge --no-ff upstream-sync-YYYYMMDD
```

Full `cargo ndk build` + `ANDROID_NDK_HOME=<ndk> gradle assembleRelease`
(gradle's buildZdExec/buildAskpassHelper tasks shell out to cargo-ndk and
need the env var too), install on a device, and run a real smoke pass
(render, terminal spawn, a git operation, an HTTPS fetch) before the next
public release.

## When upstream refactors a file we patch

Worst case: upstream renames or restructures a crate we modify. Two options:

1. Re-port our patch to the new structure (preferred).
2. Drop the patch if upstream's refactor obsoletes our need for it. Rare, but it happens. See e.g. Phase 8b's deletion of `termux_bootstrap.rs` once the bootstrap patches moved to the [zdroid-bootstrap](https://github.com/Dylanmurzello/zdroid-bootstrap) repo.

Either way: update the corresponding workaround doc under `crates/gpui_android/docs/workarounds/`.

## When in doubt

`git log --oneline upstream/main..HEAD` shows everything we've added on top of upstream. If you're unsure whether a file is ours or theirs, that log is the source of truth. (The count grows every cycle — ~350 commits at the 2026-08 merge — so run the log rather than trusting this sentence.)
