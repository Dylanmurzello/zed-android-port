# `documentLaunchMode="always"` implies Intent flags

**Status:** Active
**Phase / Commit:** L7
**Files:**
- `crates/gpui_android/examples/zed_android/android/app/src/main/AndroidManifest.xml`
- `MainActivity.launchExtraWindow`

## Problem

Initial L7 implementation set BOTH manifest `documentLaunchMode="always"`
AND explicit Intent flags `FLAG_ACTIVITY_NEW_DOCUMENT | FLAG_ACTIVITY_MULTIPLE_TASK | FLAG_ACTIVITY_NEW_TASK`
when launching ExtraWindowActivity. Plausible reading: "be explicit, set
both."

Symptom under DeX freeform windowing: launching ExtraWindowActivity caused
**MainActivity to be backgrounded** mid-tap. The new task with the extras
window came up correctly, but the primary Workspace window flickered to
the background then came back.

## Constraint

Per [Android docs](https://developer.android.com/guide/components/activities/recents):

> `documentLaunchMode="always"` is equivalent to setting both the
> `FLAG_ACTIVITY_NEW_DOCUMENT` and `FLAG_ACTIVITY_MULTIPLE_TASK` flags in
> the activity's intent.

So they're the same — but not strictly redundant. When BOTH the manifest
attribute AND the explicit flags are set, plus `FLAG_ACTIVITY_NEW_TASK`
piled on, the OS sees a launch request that's "very emphatically a new
document, very emphatically a new task" — and in DeX freeform windowing
that triggers task transition logic that reorders the existing task stack
(backgrounding MainActivity in the process).

## Solution

Pick **one** path. We picked manifest:

```xml
<activity android:name=".ExtraWindowActivity"
          android:documentLaunchMode="always"
          android:taskAffinity="com.zdroid.extra"
          ...>
```

And dropped the explicit flags from the JNI Intent build:

```rust
// `documentLaunchMode="always"` on the manifest already implies
// FLAG_ACTIVITY_NEW_DOCUMENT | FLAG_ACTIVITY_MULTIPLE_TASK, so we don't
// set them here — setting them additionally was causing MainActivity
// to be backgrounded under DeX freeform windowing.
env.call_method(
    &main_activity,
    "startActivity",
    "(Landroid/content/Intent;)V",
    &[JValue::Object(&intent)],
)?;
```

## Why this works

The manifest path produces the same task semantics (each launch = new
document task) without the explicit-flag DeX-specific transition logic
that backgrounded MainActivity.

Also makes the policy easier to reason about: one source of truth for
"how does this Activity launch?" lives in the manifest. The Rust side just
fires startActivity and trusts the declared launch mode.

## Failure mode if regressed

- Setting `FLAG_ACTIVITY_NEW_DOCUMENT | FLAG_ACTIVITY_MULTIPLE_TASK | FLAG_ACTIVITY_NEW_TASK`
  on the Intent again: MainActivity flickers to background on every "Open
  Settings" tap.
- Removing `documentLaunchMode="always"` from manifest without re-adding
  Intent flags: ExtraWindowActivity doesn't open in a separate task →
  it stacks on MainActivity's task → no separate freeform window.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
