# Maintaining this fork

This repository builds Zdroid from [Dylanmurzello/zed-android-port](https://github.com/Dylanmurzello/zed-android-port) plus newer [Zed](https://github.com/zed-industries/zed) releases, and publishes its own signed APKs. The goal is to stay mergeable with Dylan's repo, so fork-only changes live in new files where possible:

- `FORK.md` (this file)
- `.github/workflows/zdroid_apk.yml` (APK build and release)
- `.github/dependabot.yml` (keeps the pinned actions current)

Everything in [UPSTREAM_MERGE.md](UPSTREAM_MERGE.md) and [RELEASING.md](RELEASING.md) still applies, except where this file says otherwise.

## Maintenance mode

Since 0.11 the fork has the features its owner needs, and the regular work is taking new Zed releases: [Syncing a Zed release](#syncing-a-zed-release), then [Each release](#each-release). Before a sync, check [BACKLOG.md](BACKLOG.md)'s "Next Zed sync" for what the release needs. Afterwards, try the smoke checklist in [RELEASING.md](RELEASING.md) on the tablet. The parts of this file about Dylan's repository only matter if he publishes again.

## Branches

- `main` is this fork's contribution to `Dylanmurzello/zed-android-port`: `zdroid` without what only applies to this fork (see [Contributing to Dylan's repo](#contributing-to-dylans-repo)), and with Dylan's version numbers. The pull request to his repository comes from `main`, so pushing to `main` updates it. Don't develop on `main`; changes land on `zdroid` first. Dylan's own branch is `dylan/main` (the `dylan` remote).
- `zdroid` is the default branch and holds everything this fork adds: Zed syncs, fixes, CI, and releases. `git diff dylan/main...zdroid` shows the fork's whole delta.

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
git -c merge.ours.driver=true merge v1.28.1
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
  git diff --stat v1.28.1 HEAD -- . ':!crates/gpui_android' ':!crates/zdroid_runtime' ':!.github' ':!*.md' ':!Cargo.lock'
  ```
- The Android app can't link the `zed` crate, which is a binary, so it copies the startup code it needs: its `src/zed.rs` copies `crates/zed/src/zed.rs`, its `src/lib.rs` copies a few functions from `crates/zed/src/main.rs`, and `app_menus` in its `src/menu_bar.rs` copies `crates/zed/src/zed/app_menus.rs`. (`crates/zed/src/zed/migrate.rs` is included as it is.) The merge doesn't touch the copies, so port what the release changed in the originals, comparing the previous sync's tag with the new one:

  ```sh
  git diff v1.27.0 v1.28.1 -- crates/zed/src/main.rs crates/zed/src/zed.rs crates/zed/src/zed/app_menus.rs
  ```
- Go through [crates/gpui_android/docs/ahead-of-upstream.md](crates/gpui_android/docs/ahead-of-upstream.md), the features Zdroid implements ahead of Zed. If the release covers one, take upstream's and delete ours.
- Push the sync branch and open a pull request into `zdroid`. The **Zdroid APK** check builds the full APK. It covers the `cargo ndk` gate, Gradle, and the bundled helper binaries.
- Never `git push --tags`. A local clone carries every Zed `v1.x.y` tag, and those match the release trigger.

## Syncing Dylan's repo

Merge without the `ours` driver (see above), so Dylan's changes to fork-owned files get a normal 3-way merge instead of being dropped:

```sh
git fetch dylan
git checkout -b dylan-sync zdroid
git merge dylan/main
# resolve conflicts, build, commit, then open a pull request into zdroid
```

Once Dylan has merged `main` (0.4.3 took 0.13.1's), his branch carries `main`'s removals too, and the merge applies them to `zdroid`: the fork-only files listed under [Contributing to Dylan's repo](#contributing-to-dylans-repo) come back deleted or as modify/delete conflicts, and the fork sections and FORK.md references go. Put them back before committing:

```sh
git checkout zdroid -- .github/workflows/zdroid_apk.yml .github/dependabot.yml \
  .github/release .github/scripts/zdroid-release-notes.js FORK.md .rules
# README.md: keep zdroid's side of the conflict in its fork section
git diff zdroid | grep -n '^-.*\(FORK\.md\|zdroid_apk\)'    # references main dropped: restore each
```

Fork-specific changes to keep while resolving:

- `ZDROID_RELEASE_REPO` in `crates/gpui_android/src/updater.rs`. Without it, the in-app updater follows Dylan's releases, and those are signed with a different key.
- `versionCode`/`versionName` in `crates/gpui_android/examples/zed_android/android/app/build.gradle.kts`. Take whichever is higher.
- Take Dylan's small edits to Zed's crates as he wrote them, even though each one adds to what a Zed sync can conflict in: staying with his code matters more than a few lines. 0.4.3's came in this way (`remote_connection.rs`, `pane_group.rs`, `workspace.rs`, and `settings_ui`'s dependency on `zdroid_runtime`). Move one into fork-owned code only when it conflicts at a Zed sync and the change can live outside the Zed file. Send that move to Dylan first, and keep it on `zdroid` once he has merged it, so the two repositories don't drift apart. Where code moved out of Zed's crates lives, and where more of the same goes:
  - spawning, the files dir spellings and the per-spawn PATH (once his `gpui_util`, `util::command`, `util::env`): `crates/zdroid_hooks`;
  - the language server restarts (once his `lsp_store.rs`): the app's `src/lsp_watchdog.rs`;
  - Android settings (once his `workspace_settings.rs`, `default.json`, `page_data.rs`): the app, as `android_input` and `android_runtime` are, and `crates/settings_ui/src/page_data/android.rs`;
  - app code: the app's copies of Zed's startup code (`src/zed.rs`, `src/lib.rs`).
- Dylan's engine crates, `crates/zd_engine_core` and `crates/zd_loader`, are unformatted, and their `rustfmt.toml` keeps CI's format check off them. Take his versions as they are, except for two fixes the fork made and sent to him, which his next version may carry in his own form:
  - a guest's `execve` passes its signal mask to the new image as `ZD_SIGMASK`, which `main.rs` applies before the jump (exec'd programs started with every signal blocked);
  - the refusal log reads the call's own `x0`, kept before dispatch (it dereferenced the `-errno` result).
- `async-task` isn't vendored: the revision Zed pins has his fix.

## Contributing to Dylan's repo

`main` carries `zdroid` to Dylan's repository, minus what only applies to this fork:

- the APK workflow and Dependabot: `.github/workflows/zdroid_apk.yml`, `.github/dependabot.yml`
- the release notes files: `.github/release/`, `.github/scripts/zdroid-release-notes.js`
- this file, the "Zdroid fork" section at the top of `.rules`, and the references to this file elsewhere
- the "Zdroid fork" section at the top of `README.md`, down to the line under which Dylan's README starts
- `versionCode`/`versionName`: `main` keeps Dylan's, for him to bump when he releases

To update it after a release, merge `zdroid` into `main`, keeping that list out (add to the list when the fork gains another file of its own):

```sh
git checkout main
git merge zdroid    # modify/delete conflicts in the files above: keep them deleted;
                    # build.gradle.kts: take zdroid's file with main's version numbers;
                    # README.md, a conflict in its fork section: keep main's side
git rm -r --ignore-unmatch .github/workflows/zdroid_apk.yml .github/dependabot.yml \
  .github/release .github/scripts/zdroid-release-notes.js FORK.md
# remove the "# Zdroid fork" sections from .rules and the top of README.md, and `git grep FORK.md` for new references
git commit
git diff zdroid main --stat    # only the files above and the version numbers
git push origin main           # updates the pull request on Dylan's repository
```

When Dylan merges the pull request, fast-forward `main` to `dylan/main` (`git merge --ff-only dylan/main` on `main`) and sync `zdroid` from it as in [Syncing Dylan's repo](#syncing-dylans-repo). His version numbers then become `main`'s.

Keep `main`'s history: its Zed tag merges are real merges, which later Zed syncs in Dylan's repository depend on, so the pull request should be merged with a merge commit, not squashed.

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
   - **Changes**: the `Release Notes:` bullets of the pull requests merged into `zdroid` since the previous release, each linked to its pull request, the way Zed builds its notes. `N/A` entries are left out, so write these bullets for users. Pre-releases don't count as previous releases, so when a pre-release fails its checks (step 5), the release that replaces it lists its changes too. A fix committed straight to `zdroid` has no pull request, so describe it in the Highlights.
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

The same workflow checks formatting (`cargo fmt --all`, which covers the fork's edits to Zed's crates, plus `cargo fmt` in the app and in `askpass-helper`) and runs the host tests of those that build outside Android (`cargo test -p zdroid_hooks -p zdroid_runtime -p gpui_android`); a release waits for both. Every Cargo build in it, the helpers' included, uses `--locked`: update a lockfile in the pull request that needs it, not in CI.

The workflow ignores pull requests and pushes that only change docs (`paths-ignore`). On a pull request GitHub checks that against the whole diff, so once one has code, a later docs commit builds the APK again. Put `[skip ci]` in the message of such a commit, the last one of the push: GitHub then starts no run, and a build already running carries on. Use it only on commits that change no code.

1. Open the pull request's **Checks** tab, open the **Zdroid APK** run, and download the `zdroid-apk` artifact. GitHub delivers it as a `.zip`; unzip it to get the APK.
2. Install it over the current release. Same package and key means it installs as an update and keeps your data.
3. Keep `versionCode` in mind. Android refuses to install a lower `versionCode` over a higher one without uninstalling, which deletes the app's data. Bump the version in the pull request itself, so the test build and the release made from it share a `versionCode` and the release installs cleanly over the test build.

Test builds can't use a separate package name such as `com.zdroid.dev` to install side by side: the release's document provider (`com.zdroid.documents`) would clash, and the runtime binds `com.zdroid`'s data paths into its guest.

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
- **Tests of `gpui_android`:** the crate only builds for Android, so its tests run on the tablet. In the build container, run `cargo ndk -t arm64-v8a -P 31 test -p gpui_android --no-run --locked` with `CARGO_TARGET_DIR` set to the app's `target`, then `adb push` the newest `target/aarch64-linux-android/debug/deps/gpui_android-*` binary to `/data/local/tmp`, run it there and delete it. CI doesn't run them. `failed_import_removes_only_its_own_destination` fails in its setup there: Android doesn't let adb's shell bind the Unix socket it creates.
- **Screenshot:** `adb -s "$device" exec-out screencap -p > screen.png`. It captures whatever is on the tablet's screen, so ask its owner first.
- **A mouse or trackpad without hands:** `adb -s "$device" shell uinput -` reads JSON from stdin and registers a virtual device, whose events go through Android's real input stack, so a virtual trackpad gets `TouchpadInputMapper`, tap to click and classified gestures, and the device goes away when stdin closes. Register one with `{"id":1,"command":"register","name":"test-pad","vid":"0x1d6b","pid":"0x7a02","bus":"usb","configuration":[...],"abs_info":[...]}`, copying the real pad's keys, axes and properties from `getevent -pl <device>`, and send `{"id":1,"command":"inject","events":[type,code,value,...]}` with `{"id":1,"command":"delay","duration":<ms>}` between frames. Delays add to a schedule that doesn't advance while the device is idle, so after a pause add one long enough to catch up, or the next gesture arrives all at once; `{"id":1,"command":"sync","syncToken":"x"}` prints the token when everything before it has been sent. To check the results, `dumpsys input` shows the cursor Android was asked for (`requestedPointerType`) and the pointer's position (`hoveringPointers`, in the panel's unrotated coordinates, with stale entries for other windows). A virtual pad doesn't stand in for the real one's firmware or palm handling. A keyboard or a touchscreen (`INPUT_PROP_DIRECT`) registers the same way, and unlike `input`'s events theirs pass the stage that fades the cursor, whose `alpha` is under `MouseCursorController`.
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
