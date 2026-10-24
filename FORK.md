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
git remote add dylan https://github.com/Dylanmurzello/zed-android-port
git config merge.ours.driver true
```

## Syncing a Zed release

Follow [UPSTREAM_MERGE.md](UPSTREAM_MERGE.md), with these differences:

- Merge a stable release tag instead of `upstream/main`:

  ```sh
  git fetch upstream tag v1.25.3 --no-tags
  git checkout -b upstream-sync-v1.25.3 zdroid
  git merge v1.25.3
  ```

- The sync branch lands on `zdroid`, not `main`, wherever UPSTREAM_MERGE.md says `main`.

- When sweeping upstream's `.github/workflows` files, keep `zdroid_apk.yml`.
- Reseed the Android app's lockfile from the root one before the `cargo ndk` check. A stale example lock can resolve mismatched versions of crates that upstream bumped together (2026-10: `merman` vs `merman-render`):

  ```sh
  cp Cargo.lock crates/gpui_android/examples/zed_android/Cargo.lock
  ```
- Push the sync branch and open a pull request into `zdroid`. The **Zdroid APK** check builds the full APK. It covers the `cargo ndk` gate, Gradle, and the bundled helper binaries.
- Never `git push --tags`. A local clone carries every Zed `v1.x.y` tag, and those match the release trigger.

## Syncing Dylan's repo

`.gitattributes` marks `crates/gpui_android/**`, `crates/zdroid_runtime/**`, `README.md` and the other fork-owned files as `merge=ours`. That protects them during Zed merges. During a merge from Dylan's repo, though, it would silently throw away his changes to any of those files that this fork also touched. Turn the driver off for that merge so those files get a normal 3-way merge:

```sh
git fetch dylan
git push origin dylan/main:main      # keep the mirror current
git checkout -b dylan-sync zdroid
git config --unset merge.ours.driver
git merge dylan/main
# resolve conflicts, build, commit, then open a pull request into zdroid
git config merge.ours.driver true
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

The workflow rejects any APK whose certificate doesn't match `SIGNING_CERT_SHA256` in `zdroid_apk.yml`. If you ever change keys, update that value too.

### Each release

1. Bump `versionCode` (+1) and `versionName` in `build.gradle.kts`, following the versioning rules in [RELEASING.md](RELEASING.md). Commit and push to `zdroid`.
2. Write the release notes as an annotated tag. The first line is the subject (`Zdroid X.Y.Z`); the rest becomes the release body. Use the `Install`, `Highlights` (or `Fixes`), and `Caveats` sections from RELEASING.md. The workflow appends `Verification` (APK and certificate SHA-256, commit, provenance) itself.

   ```sh
   # --cleanup=whitespace keeps the `### ...` headings; the default strips `#` lines.
   git tag -a v0.4.1 --cleanup=whitespace -F /path/to/notes.md
   git push origin v0.4.1
   ```

3. The workflow builds the APK, attests its provenance, and publishes a **pre-release** with `Zdroid-X.Y.Z.apk` attached. A tag that doesn't match `versionName` fails right away.
4. Install the pre-release on a device and run the smoke checklist in RELEASING.md.
5. If it passes, edit the release on GitHub: clear **Set as a pre-release** and tick **Set as the latest release**. The in-app updater only follows the latest release. If it fails, fix it on `zdroid` and cut a new PATCH tag. Don't delete or re-upload releases.

To verify a downloaded APK:

```sh
gh attestation verify Zdroid-X.Y.Z.apk --repo ilkersigirci/zed-android-port
```
