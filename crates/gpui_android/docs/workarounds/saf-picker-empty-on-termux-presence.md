# SAF picker renders empty when Termux's DocumentsProvider is in a degraded state

**Status:** Active (external-app issue, recovery procedure documented)
**Phase / Commit:** Diagnosed during L9 (2026-05-05)
**Files:** None on our side. Recovery is `pm disable-user --user 0 com.termux && pm enable com.termux`.

## Symptom

`ACTION_OPEN_DOCUMENT_TREE` (and `ACTION_OPEN_DOCUMENT`, `ACTION_CREATE_DOCUMENT`)
fires the system DocumentsUI picker (`com.google.android.documentsui`),
but the picker lands at a top-level "Files on \<Device Name\>" /
"No items" empty view with the prompt **"Can't use this folder / To
protect your privacy, choose another folder"** at the top — regardless
of `EXTRA_INITIAL_URI`. **Use this folder** is greyed. The hamburger
sidebar drawer renders but its entries are unreachable.

This persists across `pm clear com.google.android.documentsui` and
`pm clear com.sec.android.app.myfiles` — neither flushes the bad state.
It also reproduces with **bare `adb shell am start -a
android.intent.action.OPEN_DOCUMENT_TREE`**, proving it's not Zed's
intent at fault.

Logcat shows
```
W ExternalStorage: Error in checking file equality check.
```
during the picker's root enumeration, which is the AOSP
`ExternalStorageProvider.isRestrictedPath`'s `Files.isSameFile()`
catch-all — but that warning is the *symptom*, not the cause.

## Root cause

Termux ships a `DocumentsProvider` (`com.termux.documents`, class
`TermuxDocumentsProvider`) registered against authority
`com.termux.documents` so other apps can browse Termux's `$HOME` via
SAF. When DocumentsUI builds its root list at picker startup, it queries
**every** registered SAF provider's `queryRoots`. If Termux's provider
hangs, throws, or returns a corrupt cursor, Mainline DocumentsUI's
defensive bailout path renders the empty Devices/overview view for the
entire picker session — not just for Termux's pane.

This was diagnosed by:
1. Confirming the picker is broken bare (no app intermediation, no
   `EXTRA_INITIAL_URI`).
2. `dumpsys content` shows `com.termux.documents/root` registered.
3. `pm disable-user --user 0 com.termux` → relaunch picker → picker
   renders /sdcard normally with all subfolders and **Use this folder**
   enabled.
4. `pm enable com.termux` (re-enable) → picker still works. The
   disable→enable cycle reset Termux's provider state.

The trigger we never definitively identified, but the most likely cause
is Termux's provider process getting force-stopped or its `onCreate`
crashing once on a corrupted bootstrap (Termux's `$HOME` having a
broken symlink, missing perms, etc.) — DocumentsUI then caches the
failure for the lifetime of the provider process.

## Recovery (manual)

If a user reports the SAF picker on Android 13+ with Termux installed
showing **"No items / Can't use this folder"** regardless of which
sidebar entry they tap and what initial URI is set:

```sh
adb shell pm disable-user --user 0 com.termux
adb shell pm enable com.termux
```

Or via the device UI: Settings → Apps → Termux → Disable → re-enable.

**Don't uninstall Termux** — the user's `$HOME` lives in the package
data and uninstall wipes it. Disable/enable preserves data.

## Why we don't (yet) auto-fix this in Zed

- We can't run `pm disable-user` from a non-privileged Android app —
  it's a shell-level operation. With Magisk + `su` we could, but
  shipping Magisk-only code paths is out of scope.
- We can't disable our own DocumentsProvider in the same way and
  expect Termux to behave differently — the bug is in Termux's
  provider process, not ours.
- Detecting the broken state from inside Zed would require querying
  every SAF root via `ContentResolver` and timing each — invasive,
  flaky, and still doesn't *fix* anything; we'd just surface a banner
  saying "your SAF picker is broken, run this `adb` command."

The cleanest mitigation we ship is good docs (this file) plus the
Open menu's behavior of falling back to in-app navigation (the welcome
page lists recent projects without going through SAF, so the user can
still get to them and to known shared paths even when the picker is
broken).

## See also

- [projects-workspace-import.md](projects-workspace-import.md) — the
  picker call site
- [android-noexec-mount.md](android-noexec-mount.md) — what Zed does
  with paths the picker hands back, regardless of which provider
  served them
