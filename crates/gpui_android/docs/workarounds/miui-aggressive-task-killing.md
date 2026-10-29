# MIUI / HyperOS aggressively kills backgrounded Zed

**Status:** Active (third-party OS issue, recovery + mitigation documented)
**Phase / Commit:** Diagnosed during L9 cross-device testing on Xiaomi Mi 10 (Android 13, MIUI)
**Files:** None on our side. User-facing setting change.

## Symptom

On Xiaomi devices running MIUI / HyperOS, opening Zed and then either
firing the SAF picker (`Open` menu), tapping any system dialog
(permission prompt, picker), or pressing Home backgrounds Zed and the
OS kills the process within seconds. Returning to Recents shows the
Zed card, but tapping it cold-starts the app again because the process
was killed — the user perceives this as "the app just exits".

logcat from the device confirms the cause:

```
SmartPower.com.zdroid/10377: background->died(1134ms)
  R(process died ) adj=900.
ActivityManager: Killing 16469:com.zdroid/u0a377 (adj 900):
  stop com.zdroid due to from process:17503
SmartPower: com.zdroid/10377 state=inactive adj=900
  proc size=1 move to died process died
```

`SmartPower` is MIUI's proprietary battery optimizer; it kills any
non-whitelisted process whose `oom_adj` rises above 900 (background
cached). The threshold is dramatically more aggressive than stock AOSP,
where backgrounded apps usually live for minutes-to-hours before
reclaim.

## Root cause

Not Zed. MIUI / HyperOS ships a `SmartPower` service that ranks all
running apps by user-engagement score and force-stops anything not
on its always-allow list. New installs default to **Restricted**
unless the user explicitly opts the app into **No restrictions** in
the per-app battery settings.

This affects every Android app, not just ours. It's particularly
disruptive for Zed because:

1. Re-launching after a kill goes through the whole boot path again
   (the runtime's files persist, but the process doesn't, so we re-set
   up env / re-attach JNI / reopen the workspace), and the language
   servers and terminals it ran are gone.
2. The SAF picker spawning is enough to background us (DocumentsUI
   is in another process), so MIUI kills us mid-pick.

## Recovery (user-facing)

In **Settings → Apps → Manage apps → zed_android**:

1. **Battery saver → No restrictions**
2. **Other permissions → Display pop-up windows while running in the
   background → Allow** (lets the SAF picker return cleanly)
3. **Autostart → Allow** (only relevant if user wants Zed to launch
   on boot, not strictly required for this issue)

Alternatively, in the Recents view, swipe down on the Zed card and
tap the **lock icon** — that pins it in memory and tells SmartPower
to skip it.

## Why we don't auto-fix this

- The app can't change its own battery-optimization status without
  user intervention. Android's API
  (`PowerManager.requestIgnoreBatteryOptimizations`) opens a system
  dialog, but MIUI's SmartPower is a separate restriction layer that
  the AOSP API doesn't address.
- Showing a first-launch banner saying "you may need to disable MIUI
  battery saver" would be needed for every Xiaomi/Redmi/POCO user. We
  could detect MIUI via `getprop ro.miui.ui.version.name` from JNI,
  but that's premature — let users hit the issue, point them at this
  doc.

## See also

- Confirmed working flows on this device after the workaround:
  noexec banner → Trust-style dialog → Copy → reopened project.
- Related: when SAF picker fires, our gpui surface stays alive
  (process not killed) for the duration of the picker on stock AOSP /
  Pixel / Samsung. MIUI's aggressive killing is the only OS where
  the picker round-trip kills us reliably.
