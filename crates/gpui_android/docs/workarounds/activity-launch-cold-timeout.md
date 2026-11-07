# Cold Activity launch timeout (4 seconds)

**Status:** Active
**Phase / Commit:** L7
**Files:** `crates/gpui_android/src/multi_window.rs` (`ACTIVITY_LAUNCH_TIMEOUT`)

## Problem

`Platform::open_window` is synchronous in gpui's contract — it must return
a `Box<dyn PlatformWindow>` before the App update completes. Our
`open_extra_window` blocks the gpui game thread on a oneshot waiting for
JNI `surfaceCreated` after launching ExtraWindowActivity.

If the launch stalls (system busy, OOM, dex-mode transition logic), the
game thread freezes forever. At ~5 seconds Android fires an ANR
(Application Not Responding) dialog.

## Constraint

Cold ExtraWindowActivity start in DeX freeform windowing on Snapdragon 8
Gen 2 measured at:

| Trial | Cold launch latency |
|---|---|
| 1 | 530ms |
| 2 | 560ms |
| 3 | 2030ms |
| 4 | 2070ms |

The 2-second outliers were a deadlock, not a slow system. Outside DeX the
new Activity covers the main one, so Android pauses the main Activity
first, and GameActivity's `onPause` holds the UI thread until the game
thread acknowledges `APP_CMD_PAUSE`, for at most 2 seconds. The game
thread was sleeping in this wait loop, so it never did, and the new
Activity, which starts on the same UI thread, only got its surface once
GameActivity gave up (logcat: `android_app_set_activity_state timed out
waiting for cmd 14`). Opening Settings froze Zdroid for 2 seconds every
time. The wait loop now polls android-activity's events instead of
sleeping, which acknowledges the pause right away (see
`AndroidPlatform::handle_main_event_while_opening_window`).

A 5s+ timeout still risks the game thread freeze the user notices as input
lag.

Warm reopens (Activity already alive in Recents from a recent open):
~50-100ms. Trivial.

## Solution

Hard cap the wait at **4 seconds**, return `Err` on timeout:

```rust
const ACTIVITY_LAUNCH_TIMEOUT: Duration = Duration::from_millis(4000);

let deadline = std::time::Instant::now() + ACTIVITY_LAUNCH_TIMEOUT;
loop {
    match rx.try_recv() {
        Ok(Some(native_window)) => return Ok(native_window),
        Ok(None) => { /* not ready yet */ }
        Err(_) => {
            pending_table().as_mut().and_then(|m| m.remove(&window_id));
            bail!("extra surface creation channel dropped (sender canceled)");
        }
    }
    if std::time::Instant::now() >= deadline {
        pending_table().as_mut().and_then(|m| m.remove(&window_id));
        bail!(
            "ExtraWindowActivity startup exceeded {}ms",
            ACTIVITY_LAUNCH_TIMEOUT.as_millis()
        );
    }
    // Polls the main Activity's lifecycle commands; see above.
    wait(Duration::from_millis(8));
}
```

On timeout the gpui caller gets a clean error ("couldn't open Settings,
try again"). Better than a 4s freeze followed by a ghost window.

## Why this works

- 4s covers slow cold starts with headroom.
- 4s is below Android's 5s ANR threshold for input dispatch.
- Warm reopens are 50-100ms so the cap only matters on first cold open per
  process lifetime.
- If we genuinely hit it, the user retries — the second tap is warm and
  will succeed.

## Failure mode if regressed

- **Timeout too short** (e.g. back to 500ms): every cold launch fails
  spuriously; user sees "couldn't open" on first tap.
- **Timeout too long** (e.g. 8s): game thread freezes longer than 5s on a
  stalled launch → Android fires ANR dialog.
- **No timeout**: stalled launch freezes editor forever; user force-stops.

## See also

- [`futures::oneshot::Receiver::try_recv` semantics](futures-oneshot-tryrecv-semantics.md) — the `try_recv` semantics this loop relies on
- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
- L7g (deferred): split `WgpuRenderer` creation from surface presentation
  so `open_extra_window` returns synchronously. Replaces this timeout with
  proper async open.
