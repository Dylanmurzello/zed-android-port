# JNI exception clear after error

**Status:** Active
**Phase / Commit:** L7
**Files:** `crates/gpui_android/src/multi_window.rs` (`launch_extra_activity` wrapper)

## Problem

When a JNI call fails (e.g. `Class.forName` throws `ClassNotFoundException`),
the `jni` crate surfaces the failure as a Rust `Result::Err`. We propagate
via `?` and return.

But the JNI environment still has a **pending Java exception** at that
point. The next JNI call from anywhere — a logger, a finalizer, even a
pure-read like `GetObjectClass` — trips:

```
JNI DETECTED ERROR IN APPLICATION: JNI GetObjectClass called with pending
  exception java.lang.ClassNotFoundException: com.zdroid.ExtraWindowActivity
```

…and ART aborts the process. Our clean Rust error never reaches the user;
they see "Zed crashed" instead.

## Constraint

JNI's contract requires you to either handle a thrown Java exception (catch
+ clear) or rethrow before any subsequent JNI call. The `jni` crate doesn't
auto-clear exceptions on `?` propagation — it can't, because the caller may
want to inspect the exception object before clearing.

`AttachGuard` doesn't auto-clear on Drop either. Pending exceptions persist
until explicitly handled, even across function boundaries on the same
thread.

## Solution

Wrap any JNI-heavy function in a thin outer function that clears the
exception state before returning, regardless of inner success or failure:

```rust
fn launch_extra_activity(
    android_app: &AndroidApp,
    window_id: u64,
    bounds: Option<LaunchBounds>,
) -> Result<()> {
    let vm = unsafe { JavaVM::from_raw(android_app.vm_as_ptr().cast())? };
    let mut env = vm.attach_current_thread()?;
    let result = call_into_java(&mut env);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    result
}
```

The pattern: outer attaches, inner does the work, outer clears + returns.
Apply to anywhere we make JNI calls that can throw — `launch_extra_activity`,
`set_extra_activity_title`, etc.

## Why this works

`exception_check` is itself a side-effect-free read of the JNI thread state.
`exception_clear` resets the pending-exception slot. After `clear`, the
thread is safe for further JNI calls. We don't lose the original Rust
error — that's already captured in `result`.

## Failure mode if regressed

- App crashes with `JNI DETECTED ERROR IN APPLICATION: ... called with
  pending exception ...` whenever a JNI call fails and any subsequent JNI
  work happens before process exit.
- Recurring crash on every "Open Settings" tap if e.g. `loadClass` ever
  fails for some reason.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
