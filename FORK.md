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

`.gitattributes` marks `crates/gpui_android/**`, `crates/zdroid_runtime/**`, `README.md` and the other fork-owned files as `merge=ours`. Don't enable the driver in your git config: with it on, *every* 3-way merge of those files keeps the current branch's version silently. That covers rebases and cherry-picks of this fork's own stacked branches and merges from Dylan's repo, not just Zed merges. (2026-10: a rebase dropped an entire commit this way.) Enable it only for the Zed merge itself:

```sh
git -c merge.ours.driver=true merge v1.26.2-pre
```

If an earlier setup enabled it, run `git config --unset merge.ours.driver`.

## Syncing a Zed release

Follow [UPSTREAM_MERGE.md](UPSTREAM_MERGE.md), with these differences:

- Merge a release tag instead of `upstream/main`. Preview tags (`vX.Y.Z-pre`) work as well as stable ones; the fork took v1.26.2-pre without waiting for 1.26 to reach stable:

  ```sh
  git fetch upstream tag v1.26.2-pre --no-tags
  git checkout -b upstream-sync-v1.26.2-pre zdroid
  git -c merge.ours.driver=true merge v1.26.2-pre
  ```

- The sync branch lands on `zdroid`, not `main`, wherever UPSTREAM_MERGE.md says `main`.
- Release tags set `crates/zed/RELEASE_CHANNEL` to `stable` or `preview`, and the merge takes that without a conflict. Put it back to `dev`. The Android build relies on the Dev channel: it allows the newest extension API and keeps workspace state in the same database scope as earlier releases. For SSH projects, Android's Dev funnel (`remote/src/transport/ssh.rs`, `auto_update/src/auto_update.rs`) downloads the remote server matching the Zed version in `crates/zed/Cargo.toml`, which the app reports as its own version, from Zed's stable channel or, failing that, its preview channel. That only works when the sync is a published release tag.

  ```sh
  git checkout dylan/main -- crates/zed/RELEASE_CHANNEL
  ```

- Remove the files upstream added under `.github/` that would act on the fork (workflows, actions, `renovate.json`), and keep `zdroid_apk.yml`. Keep every other upstream file, including ones the Android build doesn't use: deleting them only causes conflicts on the next sync.
- `.rules` is upstream's file for AI agents (`CLAUDE.md`, `AGENTS.md` and `GEMINI.md` link to it), with two fork changes to keep when the merge touches it:
  - The **Zdroid fork** section at the top, the fork's rules for agents. Edit fork rules there, not in tool-specific files, so every agent reads them.
  - Upstream's README review-banner rule (the line starting `* HARD RULE: When modifying any source files`) stays deleted. It makes agents add a banner to `README.md` that only a human may delete, which suits Zed's own pull requests but means a manual edit on every pull request here.
- Reseed the Android app's lockfile from the root one before the `cargo ndk` check. A stale example lock can resolve mismatched versions of crates that upstream bumped together (2026-10: `merman` vs `merman-render`):

  ```sh
  cp Cargo.lock crates/gpui_android/examples/zed_android/Cargo.lock
  ```

  The check then drops the packages the app doesn't use from that copy. Commit the pruned file: CI builds with `--locked`, which fails if the lockfile still needs changes.
- List the upstream files the fork still changes, and drop any patch the new release made unnecessary, for example because upstream fixed the same bug. Each one can conflict again on the next sync:

  ```sh
  git diff --stat v1.26.2-pre HEAD -- . ':!crates/gpui_android' ':!crates/zdroid_runtime' ':!.github' ':!*.md' ':!Cargo.lock'
  ```
- The Android app can't link the `zed` crate, which is a binary, so it copies the startup code it needs: the app's `src/zed.rs` and `src/zed/migrate.rs` copy `crates/zed/src/zed.rs` and `crates/zed/src/zed/migrate.rs`, and its `src/lib.rs` copies a few functions from `crates/zed/src/main.rs`. The merge doesn't touch the copies, so port what the release changed in the originals, comparing the previous sync's tag with the new one:

  ```sh
  git diff v1.25.3 v1.26.2-pre -- crates/zed/src/main.rs crates/zed/src/zed.rs crates/zed/src/zed/migrate.rs
  ```
- Go through [crates/gpui_android/docs/ahead-of-upstream.md](crates/gpui_android/docs/ahead-of-upstream.md), the features Zdroid implements ahead of Zed. If the release covers one, take upstream's and delete ours.
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
2. Tag the release. The workflow writes the whole release body, in this order:
   - **Install** from `.github/release/install.md`, with the version filled in.
   - **Highlights**, an optional paragraph: the annotated tag's message after its subject line (`Zdroid X.Y.Z`). Leave it out for most releases.
   - **Changes**: the `Release Notes:` bullets of the pull requests merged into `zdroid` since the previous release, each linked to its pull request, the way Zed builds its notes. `N/A` entries are left out, so write these bullets for users.
   - **Caveats** from `.github/release/caveats.md`. Pull requests keep it current: one that fixes a listed limitation removes its line, and one that adds a limitation adds one.
   - **Verification**: APK and certificate SHA-256, commit, provenance.

   ```sh
   git tag -a vX.Y.Z -m "Zdroid X.Y.Z"   # or -F notes.md, with Highlights after the subject line
   git push origin vX.Y.Z
   ```

   To preview the body before tagging (`vA.B.C` is the previous release):

   ```sh
   GITHUB_REPOSITORY=ilkersigirci/zed-android-port node .github/scripts/zdroid-release-notes.js zdroid --version X.Y.Z --previous vA.B.C
   ```

   If `git tag` says the tag already exists and it isn't this fork's, it's a local copy of Zed's: remove those as described under [Remotes](#remotes-one-time-per-clone).

3. The workflow builds the APK, attests its provenance, and publishes a **pre-release** with `Zdroid-X.Y.Z.apk` attached. A tag that doesn't match `versionName` fails right away.
4. Install the pre-release on a device and run the smoke checklist in RELEASING.md.
5. If it passes, edit the release on GitHub: clear **Set as a pre-release** and tick **Set as the latest release**. The in-app updater only follows the latest release. If it fails, fix it on `zdroid` and cut a new PATCH tag. Don't delete or re-upload releases.

### Testing a pull request

Pull requests from this repository build an APK signed with the release key, named `Zdroid-X.Y.Z-<commit>.apk`. Pull requests from forks never get the key, so they only produce the unsigned `zdroid-apk-unsigned` artifact, which won't install.

The same workflow checks formatting (`cargo fmt --all`, which covers the fork's edits to Zed's crates, plus `cargo fmt` in the app and in `askpass-helper`) and runs the host tests of those that build outside Android (`cargo test -p zdroid_runtime`); a release waits for both. Every Cargo build in it, the helpers' included, uses `--locked`: update a lockfile in the pull request that needs it, not in CI.

1. Open the pull request's **Checks** tab, open the **Zdroid APK** run, and download the `zdroid-apk` artifact. GitHub delivers it as a `.zip`; unzip it to get the APK.
2. Install it over the current release. Same package and key means it installs as an update and keeps your data.
3. Keep `versionCode` in mind. Android refuses to install a lower `versionCode` over a higher one without uninstalling, which deletes the app's data. Bump the version in the pull request itself, so the test build and the release made from it share a `versionCode` and the release installs cleanly over the test build.

Test builds can't use a separate package name such as `com.zdroid.dev` to install side by side: the Bootstrap userland has `/data/data/com.zdroid` compiled into its binaries, so terminal, git and `pkg` would break.

To verify a downloaded APK:

```sh
gh attestation verify Zdroid-X.Y.Z.apk --repo ilkersigirci/zed-android-port
```

## Debugging on the tablet

Release APKs aren't debuggable, so Android marks Zdroid's process non-dumpable and hides it in `/proc` from every other process, including Zdroid's own terminal: `top`, `ps` and `pidof` there don't list it. Inspect the running app with `adb` from a computer instead. `adb shell` can read every process's `/proc` entries and run `dumpsys`. It can't read the app's private files, since `run-as` needs a debuggable build, so read `Zed.log` with `zed: open log` in the app.

### Connecting over wireless debugging

The computer and the tablet must be on the same network.

1. On the tablet, turn on **Settings → Developer options → Wireless debugging**.
2. Pair once per computer. Tap **Pair device with pairing code**, then on the computer run `adb pair <tablet-ip>:<pairing-port>` and type the 6-digit code. The code only works while that dialog is open. If pairing fails with `protocol fault (couldn't read status message)`, run it again while the dialog is still open.
3. Connect. The port changes whenever wireless debugging restarts, so look it up each time: `adb mdns services` lists the tablet as `_adb-tls-connect._tcp <tablet-ip>:<port>`, and the Wireless debugging screen shows it under **IP address & Port**. Then run `adb connect <tablet-ip>:<port>`.
4. `adb devices -l` should list `model:SM_X910`. It may list the tablet twice, by address and by mDNS name, so pass `-s <tablet-ip>:<port>` to every command.

To revoke access, forget the computer under **Wireless debugging → Paired devices**, or turn wireless debugging off.

### Useful commands

The examples set `device=<tablet-ip>:<port>` first.

- **Logs:** `adb -s "$device" logcat | grep -E 'zed_android|zdroid|ZedDocsProvider'`. Rust logs use the `zed_android` tag at info level and above; the Kotlin side uses `zed_android_*` and `zdroid_*` tags. Crashes, including native ones such as `SIGILL`, are in `adb -s "$device" logcat -b crash -d`.
- **Install a pull request's APK:** `adb -s "$device" install -r Zdroid-X.Y.Z-<commit>.apk`. The `versionCode` rule under [Testing a pull request](#testing-a-pull-request) still applies.
- **Screenshot:** `adb -s "$device" exec-out screencap -p > screen.png`. It captures whatever is on the tablet's screen, so ask its owner first.
- **Wake-ups and CPU per thread:** run this with Zdroid open and the tablet untouched. The editor's main loop is the `android_main` thread.

  ```sh
  seconds=30
  pid=$(adb -s "$device" shell pidof com.zdroid | tr -d '\r')
  snapshot() {
    adb -s "$device" shell "cd /proc/$pid/task && for t in *; do printf '%s\t%s\t%s\t%s\n' \$t \"\$(cat \$t/comm)\" \"\$(grep ^voluntary_ctxt_switches \$t/status | tr -dc 0-9)\" \"\$(sed 's/.*) //' \$t/stat | awk '{print \$12+\$13}')\"; done 2>/dev/null" | tr -d '\r'
  }
  snapshot > /tmp/zdroid-before; sleep "$seconds"; snapshot > /tmp/zdroid-after
  awk -F'\t' -v s="$seconds" 'NR == FNR {w[$1] = $3; c[$1] = $4; next} ($1 in w) {printf "%7.1f wakeups/s %6.2f%% CPU  %s\n", ($3 - w[$1]) / s, ($4 - c[$1]) / s, $2}' /tmp/zdroid-before /tmp/zdroid-after | sort -rn | head
  ```

- **Frames the window presented:** SurfaceFlinger keeps the last 128 present times of each layer, about a second of scrolling at 120 Hz, so run this right after the thing you're measuring. Zdroid's layer only exists while its window is on screen, and its number changes each time.

  ```sh
  layer=$(adb -s "$device" shell dumpsys SurfaceFlinger --list | tr -d '\r' | sed -n 's/^RequestedLayerState{\(.*SurfaceView\[com\.zdroid\/com\.zdroid\.MainActivity\]@0(BLAST)#[0-9]*\) .*/\1/p' | head -1)
  adb -s "$device" shell "dumpsys SurfaceFlinger --latency '$layer'" | tr -d '\r' > /tmp/zdroid-latency
  period=$(head -1 /tmp/zdroid-latency)
  awk 'NR > 1 && $2 > 0 && $2 < 9e18 {print $2}' /tmp/zdroid-latency | sort -n | awk -v period="$period" '{t[NR] = $1} END {span = (t[NR] - t[1]) / 1e9; printf "%d frames over %.1f s: %.1f per second\n", NR, span, (NR - 1) / span; for (i = 2; i <= NR; i++) if (t[i] - t[i - 1] > 1.5 * period) late++; printf "%d of %d gaps were longer than one refresh (%.1f ms)\n", late, NR - 1, period / 1e6}'
  ```

  The `--list` format above is Android 16's. The first line of `--latency` output is the panel's refresh period in nanoseconds, and the second column of each following line is when a frame was presented.
- **CPU profile with function names:** release builds are profileable, and each CI build publishes its library's symbols as the `zdroid-native-symbols` artifact, also attached to releases. Use the symbols from the same build as the installed APK; they're matched by build ID. Unzip the artifact twice, since GitHub wraps it in another zip, and copy `arm64-v8a/libzed_android.so.sym` to `libzed_android.so` in a folder of its own. Record with the tablet's own `simpleperf`, because the copy the NDK's scripts push can't use the `cpu-clock` event, then symbolize with the NDK's scripts:

  ```sh
  adb -s "$device" shell simpleperf record --app com.zdroid -e cpu-clock -f 2000 -g --duration 20 -o /data/local/tmp/zdroid.data
  adb -s "$device" pull /data/local/tmp/zdroid.data
  ANDROID_SERIAL="$device" python3 "$ANDROID_NDK_HOME/simpleperf/binary_cache_builder.py" -i zdroid.data -lib <symbols folder> --ndk_path "$ANDROID_NDK_HOME"
  "$ANDROID_NDK_HOME/simpleperf/bin/linux/x86_64/simpleperf" report -i zdroid.data --symfs binary_cache --comms android_main --sort symbol --percent-limit 1
  ```

  Add `--children` to see time by caller. `simpleperf dump zdroid.data` lists each sample's thread and CPU core, which shows whether the main loop ran on the prime core (cpu7) or the slower ones.
