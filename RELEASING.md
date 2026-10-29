# Releasing Zdroid

Process for cutting a Zdroid release. Read end-to-end before doing one.

## Inviolable rules

1. **Never `--clobber` an existing release asset.** Doing so deletes the asset, which resets its download counter to zero. Cumulative download history is gone forever. Cut a new tag instead.
2. **Never delete an old release.** `/total` download badge sums across every release that exists. Deleting v0.1.0 wipes its accumulated downloads from the count. Old releases are read-only history; leave them alone.
3. **Never release without device-verifying every README-claimed feature** that the changes in this release touched. Compile + install + boot is *not* shipped. See "Smoke checklist" below.

A shell-level guardrail blocking `gh release upload --clobber` against this repo lives in `~/.zshrc` (see the `gh()` function added during v0.1.1). It refuses the destructive command before it can run. If you're on a different machine, paste the same function from your dotfiles backup before doing release work.

## Versioning

Semver-flavored, `vMAJOR.MINOR.PATCH`:

- **MAJOR** is reserved for "the user-facing flow changed materially" or "we broke compatibility with installed APKs from an earlier major" — bumping the keystore, dropping a feature, restructuring `~/projects` layout, etc. Currently 0.
- **MINOR** for any user-visible feature addition. Multiple fixes can ride along.
- **PATCH** for bug-fix-only releases. No new features.

Don't reuse a tag that shipped. A release that was pulled within the hour (deleted release and tag) is the one exception: the next build keeps the public version name and tag, and raises `versionCode` by one so a device that installed the pulled APK still updates in place (Android accepts an equal `versionCode` for a sideload, but the higher one removes any doubt). Don't add a new version with a hyphen suffix (e.g. `v0.1.1-hotfix`) — just bump PATCH.

## End-to-end flow

### 1. Make sure code is on `main` and pushed

```sh
git status   # working tree should be clean
git log --oneline origin/main..HEAD  # nothing local; if there is, push first
```

If commits since last release tag aren't on `main`, this release won't reflect them. `gh release create vX.Y.Z` defaults to the latest commit on the default branch.

### 2. Inventory the changes since last release

```sh
LAST_TAG=$(git describe --tags --abbrev=0)
git log --oneline "$LAST_TAG..HEAD" -- ':!README.md' ':!docs/'
```

Read that list. Note user-visible behavior changes vs internal/refactor commits. The user-visible ones go in release notes.

### 3. Build the release APK

```sh
cd crates/gpui_android/examples/zed_android
export ANDROID_NDK_HOME=/opt/homebrew/share/android-commandlinetools/ndk/27.0.12077973
RUSTFLAGS="-C target-feature=+fp16" \
  cargo ndk -t arm64-v8a --platform 26 -o android/app/src/main/jniLibs build --release
cd android
gradle clean assembleRelease
```

APK lands at `app/build/outputs/apk/release/app-release.apk`. ~9 minutes for a clean release build. Always `clean` for a release: an incremental package can keep the dead bytes of a replaced native library inside the zip (a debug APK measured 257 MB with 148 MB of listed entries), and `clean` also refetches the rootfs image into `build/rootfs/` (33 MB, SHA-256 checked). The gradle tasks need `ANDROID_NDK_HOME` exported, not only prefixed to the cargo line.

Confirm signing — should match the persistent zdroid-release keystore fingerprint:

```sh
APKSIGNER=$(find /opt/homebrew/share/android-commandlinetools/build-tools -name apksigner | sort -V | tail -1)
"$APKSIGNER" verify --print-certs app/build/outputs/apk/release/app-release.apk | grep -i sha-256
# expect: 25:68:A2:A3:81:2D:8B:C3:A4:E8:E7:56:68:5C:5C:F7:25:40:EA:A2:94:37:F4:C1:25:8A:0C:81:73:BF:AB:4E
```

A different cert means users would have to uninstall/reinstall to upgrade — never let that happen. If the keystore is missing, restore it from your backup before continuing.

### 4. Smoke checklist on a real device

`adb install -r app/build/outputs/apk/release/app-release.apk` over the previous release (same cert preserves data dir), then:

A device that runs debug builds carries the debug certificate, and the release-signed APK will not install over it without an uninstall, which deletes the rootfs. Do not uninstall on a device whose rootfs you want to keep: run the list below on that device with a debug build of the release commit, and run the release-signed APK itself through the install, first-launch and self-test items on the emulator or a device you can wipe.

- [ ] App boots cleanly. No `RustPanic` / `Fatal signal 11` in `adb logcat -d`.
- [ ] Welcome screen reads "Welcome to Zdroid" / "Welcome back to Zdroid". Logo is the orb mark.
- [ ] Menu bar reads "Zdroid". Settings has no Auto Update or Telemetry sections.
- [ ] **Open Project** from the welcome screen pops the SAF picker. Pick a folder, project loads.
- [ ] **Clone Repository** from the welcome screen pops the URL modal. Submit a small public repo URL. Toast appears with Cancel button. Clone completes, project opens.
- [ ] Open a folder on `/sdcard/`. Yellow Move chip appears in the title bar. Tap, confirm Copy. Toast with Cancel button shows. Cancel mid-copy: dst at `~/projects/<name>` is removed cleanly.
- [ ] On a wiped install (`adb shell pm clear com.zdroid`; never on a device whose rootfs you want to keep), the first launch extracts the rootfs behind the splash (`adb logcat -d | grep runtime_setup` shows `rootfs installed from the bundled image`), opens the All files access toggle for Zdroid, asks to allow notifications once the first child starts, the "Installing git and certificates" notification completes, and Settings → Android Runtime names the installed image.
- [ ] On that same wiped install, in the integrated terminal, the README's commands exactly as a new user types them: `apt install -y nodejs npm`, then `npm install -g @anthropic-ai/claude-code`, then `claude --version`. No question may appear on the way (a debconf menu is a seed the installer is missing, not something to answer), and `claude` must resolve in a fresh terminal. This cannot be done on the emulator (its surface hangs after the first paint) and a dev device's tuned rootfs does not count: tzdata, node and npm being present there is what hid the time zone menu from the 0.4.0 smoke.
- [ ] `pwd` in the terminal prints the directory, not `.` with a `getcwd` error, both under a project and after `cd /usr` (the kernel's spelling of the files dir must match the engine's).
- [ ] Over the previous release, `files/rootfs` is untouched, `readlink files/zd-runtime/bash` points at the new `/data/app/~~...` loader, and no `files/usr` or `files/bin/zd-exec` remains.
- [ ] `zdroid runtime: engine self test` from the command palette passes every step.
- [ ] Integrated terminal opens as `root@zdroid`. `apt install -y python3 && python3 -c 'import os; print(os.getcwd())'` prints the project dir.
- [ ] `apt install -y nodejs npm && npm install -g @anthropic-ai/claude-code && claude` runs, claude responds.
- [ ] Open a Rust project with rustup installed: rust-analyzer diagnostics appear; Home for a minute, come back, the server is the same pid (`adb shell ps -A | grep rust-analyzer`).
- [ ] Powerline-style prompt characters (`❯`, `✻`, etc.) render as glyphs not boxes (font fallback to /system/fonts).
- [ ] Touch a `.rs` file in a worktree. Editor opens, syntax highlighting active.
- [ ] Git panel opens, shows current branch / staging.

If any check fails, do **not** proceed to step 5. Triage the regression, push a fix to `main`, restart at step 3.

### 5. Tag + push

```sh
VERSION=v0.1.X
git tag -a "$VERSION" -m "Zdroid ${VERSION#v}"
git push origin "$VERSION"
```

### 6. Stage the APK + write release notes

Copy the APK to a versioned name and write notes to a temp file:

```sh
mkdir -p /tmp/zdroid-release-build
cp crates/gpui_android/examples/zed_android/android/app/build/outputs/apk/release/app-release.apk \
   "/tmp/zdroid-release-build/Zdroid-${VERSION#v}.apk"

# verify, copy these into the notes
shasum -a 256 "/tmp/zdroid-release-build/Zdroid-${VERSION#v}.apk"
git rev-parse HEAD
```

Release notes style — patch-notes form, terse:

- **No em-dashes (`—`).** Periods, colons, parens. The user has called this out repeatedly.
- **No "first public APK" / "first build of Zdroid" duplication** — the title already conveys it.
- **No License section** — GitHub renders the LICENSE file in the sidebar already.
- **Title** is just `Zdroid X.Y.Z`. No tagline.
- Sections: `Install`, `Highlights` (or `Fixes` for patch releases), `Caveats`, `Verification`. Skip `Highlights` for pure-bugfix releases; replace with `Fixes`.
- Install instructions favor sideload (file manager / download notification), with `adb install -r` as a parenthetical for dev iteration.
- The Caveats list inherits from previous release — include items still relevant. Drop items the new release fixed.

Template (replace the bracketed bits):

```markdown
### Install

Download `Zdroid-X.Y.Z.apk` below and open it on your Android device. Allow "install from unknown sources" when prompted, then tap the Zdroid icon.

For dev iteration: `adb install -r Zdroid-X.Y.Z.apk`.

This is an in-place upgrade over [previous] — same signing cert, your existing data dir is preserved.

### Fixes

* [terse bullet, lead with the user-visible change, then the why]

### Caveats

* [carry over relevant items from the previous release]

### Verification

* APK SHA-256: `<sha>`
* Signing cert SHA-256: `25:68:A2:A3:81:2D:8B:C3:A4:E8:E7:56:68:5C:5C:F7:25:40:EA:A2:94:37:F4:C1:25:8A:0C:81:73:BF:AB:4E`
* Built from commit `<short SHA>`
* Rootfs image: `ubuntu-base-26.04.1-base-arm64.tar.gz` (bundled; name and SHA-256 in `crates/zdroid_runtime/rootfs-image.properties`)
```

### 7. Cut the release

```sh
gh release create "$VERSION" \
  --repo Dylanmurzello/zed-android-port \
  --title "Zdroid ${VERSION#v}" \
  --notes-file /tmp/zdroid-release-build/release-notes.md \
  --latest \
  "/tmp/zdroid-release-build/Zdroid-${VERSION#v}.apk"
```

`--latest` is intentional — every new release should be the latest entry on the public page. The bootstrap and spawnd assets of the adapter era stay published as pre-releases for history; nothing new is hosted besides the APK.

### 8. Verify the public release works for anonymous users

```sh
URL="https://github.com/Dylanmurzello/zed-android-port/releases/download/${VERSION}/Zdroid-${VERSION#v}.apk"
curl -sIL -A "Mozilla/5.0" "$URL" | grep -E "^HTTP|^content-length"
```

Should return `302 → 200` with a content-length matching the APK byte count. If it 404s, the release didn't go public — check `gh release view "$VERSION" --json isDraft` and un-draft if needed.

### 9. After release

- The README badge / `/releases/latest` link auto-updates to point at the new version. No README changes needed unless install instructions changed.
- No need to manually update download counts — the worker / shields.io badges do it.

## Common mistakes (and what they cost)

- **Forgot to push `main` first → release tags an old commit.** Symptom: release notes don't reflect what's actually in the APK. Fix: delete the tag, push, re-tag.
- **Forgot the smoke checklist → user-visible bug ships.** Symptom: bug report on the issue tracker within hours, repo's reputation takes a hit. Fix: smoke check before EVERY release, no exceptions.
- **Used `--clobber` reflexively → counter resets.** Should be impossible if the shell guardrail is loaded. If it isn't, the rule is stop, source `.zshrc`, then continue.
- **Drafted release for "later" → actual files leak via the GitHub releases JSON API even while drafted.** Drafts aren't private; only un-published. If you need to gate visibility, keep the repo private until the release is finalized.

## What goes wrong if you ignore this guide

The numbers (downloads, stars) and the trust (cert continuity, fix-this-week cadence) compound. They can also unwind in one botched release. Slow is smooth, smooth is fast.
