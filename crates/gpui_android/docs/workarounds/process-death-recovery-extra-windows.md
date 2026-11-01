# Process-death recovery for extra windows

**Status:** Active
**Phase / Commit:** L7c
**Files:**
- `crates/gpui_android/src/multi_window.rs` (`REGISTERED_WINDOWS`, `nativeIsExtraWindowKnown`)
- `crates/gpui_android/src/platform.rs` (`open_extra_window`, `OsClosed` drain)
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/ExtraWindowActivity.kt`
- `crates/gpui_android/examples/zed_android/android/app/src/main/kotlin/com/zdroid/NativeBridge.kt`

## Problem

Android can kill a backgrounded app's process under memory pressure.
The Activity's task survives in Recents — when the user taps the task
card, Android creates a fresh process and resurrects the Activity from
its saved Intent + extras.

For ExtraWindowActivity (a Settings window's host):

1. User opens Settings → ExtraWindowActivity launches in process P1, gpui
   in P1 registers `windowId=4294967298`.
2. User backgrounds Zed for a long time.
3. Android kills P1.
4. User comes back to Recents, sees a "Zed (Settings)" card, taps it.
5. Android starts process P2, resurrects ExtraWindowActivity in P2.
6. ExtraWindowActivity's `onCreate` runs in P2 with the same windowId in
   Intent extras — **but P2's gpui knows nothing about it**. P2 may not
   even have a gpui App alive (MainActivity might not have been brought
   forward).

End-state without the workaround: ExtraWindowActivity attaches a
SurfaceView, fires JNI callbacks, but the Rust runtime has no matching
gpui Window → touches do nothing, no rendering. Ghost window.

## Constraint

We can't tell the OS "don't resurrect this Activity from Recents." We can
only react inside `onCreate`. We need a way to detect "is the gpui side
ready for this windowId?" and finish() if not.

The check has to run BEFORE any other JNI calls that depend on gpui
state being live. EXTRA_ACTIVITY_REFS isn't a useful proxy — it's set
INSIDE `nativeOnExtraActivityCreated`, which runs after the resurrection
check.

## Solution

A separate process-global `REGISTERED_WINDOWS: Mutex<Option<HashSet<u64>>>`
that gpui-side code maintains as the source of truth for "windows the gpui
runtime knows are open":

- `mark_window_registered(window_id)` — called from `open_extra_window`
  BEFORE `launch_extra_activity` (so the Activity's onCreate check passes).
- `unmark_window_registered(window_id)` — called from `OsClosed` drain
  handler after the gpui Window is torn down.
- `unmark_window_registered(window_id)` also called from each error path in
  `open_extra_window` (timeout, `attach_surface` failure).

JNI extern fn:

```rust
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeIsExtraWindowKnown<'local>(
    _env: jni::JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) -> jni::sys::jboolean {
    let window_id = window_id as u64;
    let known = registered_set()
        .as_ref()
        .map(|s| s.contains(&window_id))
        .unwrap_or(false);
    log::info!("multi_window: nativeIsExtraWindowKnown windowId={window_id} → {known}");
    jni::sys::jboolean::from(known)
}
```

ExtraWindowActivity.onCreate consults it before any other JNI work:

```kotlin
extraWindowId = intent.getLongExtra(EXTRA_WINDOW_ID, -1L)
...
if (!NativeBridge.nativeIsExtraWindowKnown(extraWindowId)) {
    Log.w(TAG, "onCreate windowId=$extraWindowId not known to Rust runtime (resurrection?); finishing")
    finish()
    return
}
NativeBridge.nativeOnExtraActivityCreated(extraWindowId, this)
// ... continue with SurfaceView setup ...
```

After process death, `REGISTERED_WINDOWS` is empty (it's a process-local
static). The check returns false → finish() → user lands back at Recents
or launcher.

### Race avoidance

The mark MUST happen before `launch_extra_activity` posts the Intent, not
after `attach_surface`. Otherwise:

- Game thread: `launch_extra_activity` (Intent fired)
- UI thread (very fast): Activity onCreate → `nativeIsExtraWindowKnown` →
  false (mark hasn't happened) → Activity finish()es itself
- Game thread: `block_on` waits forever for a surface that won't come

Symptom: every "Open Settings" tap finishes the Activity instantly. The
mark MUST be the first thing in `open_extra_window`. Failure paths
(timeout, attach_surface error) call `unmark_window_registered` to clean up.

## Why this works

`REGISTERED_WINDOWS` is per-process — fresh on every process spawn. After
death + resurrection it's empty regardless of what the resurrected
Activity's Intent extras say. So the Activity sees "you're a zombie" and
self-terminates.

## Failure mode if regressed

- **Without the check:** ExtraWindowActivity opens after process kill,
  shows a black SurfaceView with native chrome but no gpui content.
  Touches do nothing. User has to swipe-close manually.
- **Wrong race ordering** (mark after `launch_extra_activity`): every cold
  launch self-terminates. "Open Settings" never opens.
- **Forgetting `unmark_window_registered` on close:** stale entry in the
  set. If a future window reuses the same `WindowId` (rare — gpui slot
  reuse), the resurrection check passes incorrectly.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
