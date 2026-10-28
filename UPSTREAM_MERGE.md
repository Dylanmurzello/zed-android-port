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

### Startup code the app copies (silent-staleness class)

The Android app can't link `crates/zed` (a binary), so it copies the startup code it needs into `crates/gpui_android/examples/zed_android/src/zed.rs`, `src/zed/migrate.rs` and `src/lib.rs`. The merge never touches those copies: port what the release changed in `crates/zed/src/main.rs`, `zed.rs` and `zed/migrate.rs` by hand (see "Syncing a Zed release" in FORK.md).

When you resolve a conflict in one of these, ALSO check if a workaround doc under `crates/gpui_android/docs/workarounds/` covers the affected file: the doc explains why the patch is there, which makes the resolution obvious.

## Merge driver

`.gitattributes` declares `merge=ours` for owned files. Enable the `ours` driver only for the upstream merge command itself, never in your git config: with it on, rebases, cherry-picks and other merges silently keep one side of those files (see "The `ours` merge driver" in FORK.md):

```sh
git -c merge.ours.driver=true merge vX.Y.Z
```

## Performing an upstream merge

```sh
git fetch upstream main
git checkout -b upstream-sync-$(date +%Y%m%d)
git -c merge.ours.driver=true merge upstream/main
# expected:
#   - automatic merges on most files
#   - automatic ours-merge on identity files (no prompt)
#   - modify/delete conflicts on upstream GitHub workflows we deleted: `git rm` them in bulk.
#     ALSO sweep for files upstream ADDED under .github/ (workflows, actions, renovate.json)
#     since the merge-base — they merge cleanly as additions and must be removed by hand
#     (merge=ours can't help; git never invokes merge drivers for adds/deletes). The fork
#     keeps every other upstream file, even ones the Android build doesn't use.
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
2. Drop the patch if upstream's refactor obsoletes our need for it. Rare, but it happens. See e.g. how the files the app used to write into the bootstrap at startup moved into the [zdroid-bootstrap](https://github.com/ilkersigirci/zdroid-bootstrap) zip itself.

Either way: update the corresponding workaround doc under `crates/gpui_android/docs/workarounds/`.

## When in doubt

`git log --oneline upstream/main..HEAD` shows everything we've added on top of upstream. If you're unsure whether a file is ours or theirs, that log is the source of truth. (The count grows every cycle — ~350 commits at the 2026-08 merge — so run the log rather than trusting this sentence.)
