# Maintaining this fork

This repository builds Zdroid from [Dylanmurzello/zed-android-port](https://github.com/Dylanmurzello/zed-android-port) plus newer [Zed](https://github.com/zed-industries/zed) releases, and publishes its own signed APKs. The goal is to stay mergeable with Dylan's repo, so fork-only changes live in new files where possible:

- `FORK.md` (this file)
- `.github/workflows/zdroid_apk.yml` (APK build and release)
- `.github/dependabot.yml` (keeps the pinned actions current)

Everything in [UPSTREAM_MERGE.md](UPSTREAM_MERGE.md) and [RELEASING.md](RELEASING.md) still applies, except where this file says otherwise.

## Branches

- `main` is an exact mirror of `Dylanmurzello/zed-android-port:main`. Never commit to it. Update it with GitHub's **Sync fork** button, which is always a fast-forward, or with `git fetch dylan && git push origin dylan/main:main`. While it's behind, GitHub shows "This branch is N commits behind Dylanmurzello:main", which is the signal that Dylan pushed something.
- `zdroid` is the default branch and holds everything this fork adds: Zed syncs, fixes, CI, and releases. `git diff main...zdroid` shows the fork's whole delta.

To get notified when Dylan pushes, **Watch** his repository on GitHub (Custom → Releases, or All activity).

## Remotes (one-time, per clone)

```sh
git remote add upstream https://github.com/zed-industries/zed
git config remote.upstream.tagOpt --no-tags
git remote add dylan https://github.com/Dylanmurzello/zed-android-port
```

`--no-tags` keeps Zed's tags out of the clone, because Zed's own 0.x tags from before Zed 1.0 reuse this fork's version names: a local copy of Zed's `v0.8.0` blocked tagging Zdroid 0.8.0. Fetch the Zed release you sync by name instead, as in the steps below. GitHub never has Zed's tags, so this only concerns local clones.

A clone that already fetched them lists Zed's `v0.*` tags next to this fork's. Delete the local copies that match Zed's, by name and commit, so this fork's own and Dylan's stay:

```sh
git ls-remote --tags upstream 'v0.*' | grep -v '\^{}$' | awk '{print $2" "$1}' | sed 's#refs/tags/##' | sort > /tmp/zed-v0-tags
git for-each-ref --format='%(refname:short) %(objectname)' 'refs/tags/v0.*' | sort | comm -12 - /tmp/zed-v0-tags | cut -d' ' -f1 | xargs -r git tag -d
```

## The `ours` merge driver

`.gitattributes` marks `crates/gpui_android/**`, `crates/zdroid_runtime/**`, `README.md` and the other fork-owned files as `merge=ours`, and UPSTREAM_MERGE.md tells you to enable the driver once per clone. Don't: with the driver on, *every* 3-way merge of those files keeps the current branch's version silently. That covers rebases and cherry-picks of this fork's own stacked branches and merges from Dylan's repo, not just Zed merges. (2026-10: a rebase dropped an entire commit this way.) Enable it only for the Zed merge itself:

```sh
git -c merge.ours.driver=true merge v1.25.3
```

If an earlier setup enabled it, run `git config --unset merge.ours.driver`.

## Syncing a Zed release

Follow [UPSTREAM_MERGE.md](UPSTREAM_MERGE.md), with these differences:

- Merge a stable release tag instead of `upstream/main`:

  ```sh
  git fetch upstream tag v1.25.3 --no-tags
  git checkout -b upstream-sync-v1.25.3 zdroid
  git -c merge.ours.driver=true merge v1.25.3
  ```

- The sync branch lands on `zdroid`, not `main`, wherever UPSTREAM_MERGE.md says `main`.
- Release tags set `crates/zed/RELEASE_CHANNEL` to `stable`, and the merge takes that without a conflict. Put it back to `dev`. The Android build relies on the Dev channel: it allows the newest extension API and keeps workspace state in the same database scope as earlier releases. For SSH projects, Android's Dev funnel (`remote/src/transport/ssh.rs`, `auto_update/src/auto_update.rs`) downloads the stable remote server matching the Zed version in `crates/zed/Cargo.toml`, which the app reports as its own version. That only works when the sync is a published release tag.

  ```sh
  git checkout dylan/main -- crates/zed/RELEASE_CHANNEL
  ```

- When sweeping upstream's `.github/workflows` files, keep `zdroid_apk.yml`.
- `.rules` is upstream's file for AI agents (`CLAUDE.md`, `AGENTS.md` and `GEMINI.md` link to it), with two fork changes to keep when the merge touches it:
  - The **Zdroid fork** section at the top, the fork's rules for agents. Edit fork rules there, not in tool-specific files, so every agent reads them.
  - Upstream's README review-banner rule (the line starting `* HARD RULE: When modifying any source files`) stays deleted. It makes agents add a banner to `README.md` that only a human may delete, which suits Zed's own pull requests but means a manual edit on every pull request here.
- Reseed the Android app's lockfile from the root one before the `cargo ndk` check. A stale example lock can resolve mismatched versions of crates that upstream bumped together (2026-10: `merman` vs `merman-render`):

  ```sh
  cp Cargo.lock crates/gpui_android/examples/zed_android/Cargo.lock
  ```
- List the upstream files the fork still changes, and drop any patch the new release made unnecessary, for example because upstream fixed the same bug. Each one can conflict again on the next sync:

  ```sh
  git diff --stat v1.25.3 HEAD -- . ':!crates/gpui_android' ':!crates/zdroid_runtime' ':!.github' ':!*.md' ':!Cargo.lock'
  ```
- Push the sync branch and open a pull request into `zdroid`. The **Zdroid APK** check builds the full APK. It covers the `cargo ndk` gate, Gradle, and the bundled helper binaries.
- Never `git push --tags`. A local clone carries every Zed `v1.x.y` tag, and those match the release trigger.

## Syncing Dylan's repo

Merge without the `ours` driver (see above), so Dylan's changes to fork-owned files get a normal 3-way merge instead of being dropped:

```sh
git fetch dylan
git push origin dylan/main:main      # keep the mirror current
git checkout -b dylan-sync zdroid
git merge dylan/main
# resolve conflicts, build, commit, then open a pull request into zdroid
```

Fork-specific changes to keep while resolving:

- `ZDROID_RELEASE_REPO` in `crates/gpui_android/src/updater.rs`. Without it, the in-app updater follows Dylan's releases, and those are signed with a different key.
- `versionCode`/`versionName` in `crates/gpui_android/examples/zed_android/android/app/build.gradle.kts`. Take whichever is higher.

To offer a change back to Dylan, branch off `main`, cherry-pick the generic commits (not CI, this file, or version bumps), and open a pull request against his repository.

## Releasing

APKs from this repo are signed with this fork's own key, not Dylan's. Android won't install one over the other, so users switching from Dylan's build must uninstall it first. That also deletes the app's data.

### One-time setup

1. Back up `~/zdroid-signing/` (`release.keystore` and `keystore-password.txt`) somewhere safe and offline. If the key is lost, installed copies can never be upgraded in place again.
2. In the repository's **Settings → Secrets and variables → Actions**, add two repository secrets:
   - `ZDROID_KEYSTORE_BASE64`: the contents of a file made with `base64 -w0 ~/zdroid-signing/release.keystore > ~/zdroid-signing/ZDROID_KEYSTORE_BASE64.txt`. Open the file in a text editor and copy everything. Copying the output straight from a zsh terminal also picks up the `%` that zsh prints after output with no trailing newline, which breaks the secret.
   - `ZDROID_KEYSTORE_PASSWORD`: the contents of `~/zdroid-signing/keystore-password.txt`

   With the GitHub CLI, `gh secret set ZDROID_KEYSTORE_BASE64 < ~/zdroid-signing/ZDROID_KEYSTORE_BASE64.txt` (and the same for the password file) avoids copying entirely.

   The key alias defaults to `zdroid`. Override it with an Actions variable named `ZDROID_KEY_ALIAS`.
3. Make sure Actions are enabled under the repository's **Actions** tab.

Only the workflow's **Sign APK** job gets the key. The **Build APK** job builds an unsigned APK, and the sign job signs it with `apksigner`, so the key is never on disk while crate build scripts and Gradle plugins run. A wrong or badly pasted secret therefore fails at the end of the build, not the start.

The workflow rejects any APK whose certificate doesn't match `SIGNING_CERT_SHA256` in `zdroid_apk.yml`. If you ever change keys, update that value too.

### Each release

1. Bump `versionCode` (+1) and `versionName` in `build.gradle.kts`, following the versioning rules in [RELEASING.md](RELEASING.md). Commit and push to `zdroid`.
2. Write the release notes as an annotated tag. The first line is the subject (`Zdroid X.Y.Z`); the rest becomes the release body. Use the `Install`, `Highlights` (or `Fixes`), and `Caveats` sections from RELEASING.md. The workflow appends `Verification` (APK and certificate SHA-256, commit, provenance) itself.

   ```sh
   # --cleanup=whitespace keeps the `### ...` headings; the default strips `#` lines.
   git tag -a v0.4.1 --cleanup=whitespace -F /path/to/notes.md
   git push origin v0.4.1
   ```

   If `git tag` says the tag already exists and it isn't this fork's, it's a local copy of Zed's: remove those as described under [Remotes](#remotes-one-time-per-clone).

3. The workflow builds the APK, attests its provenance, and publishes a **pre-release** with `Zdroid-X.Y.Z.apk` attached. A tag that doesn't match `versionName` fails right away.
4. Install the pre-release on a device and run the smoke checklist in RELEASING.md.
5. If it passes, edit the release on GitHub: clear **Set as a pre-release** and tick **Set as the latest release**. The in-app updater only follows the latest release. If it fails, fix it on `zdroid` and cut a new PATCH tag. Don't delete or re-upload releases.

### Testing a pull request

Pull requests from this repository build an APK signed with the release key, named `Zdroid-X.Y.Z-<commit>.apk`. Pull requests from forks never get the key, so they only produce the unsigned `zdroid-apk-unsigned` artifact, which won't install.

1. Open the pull request's **Checks** tab, open the **Zdroid APK** run, and download the `zdroid-apk` artifact. GitHub delivers it as a `.zip`; unzip it to get the APK.
2. Install it over the current release. Same package and key means it installs as an update and keeps your data.
3. Keep `versionCode` in mind. Android refuses to install a lower `versionCode` over a higher one without uninstalling, which deletes the app's data. Bump the version in the pull request itself, so the test build and the release made from it share a `versionCode` and the release installs cleanly over the test build.

Test builds can't use a separate package name such as `com.zdroid.dev` to install side by side: the Bootstrap userland has `/data/data/com.zdroid` compiled into its binaries, so terminal, git and `pkg` would break.

To verify a downloaded APK:

```sh
gh attestation verify Zdroid-X.Y.Z.apk --repo ilkersigirci/zed-android-port
```
