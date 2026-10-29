# `activate()` brings extra Activity to foreground via `AppTask.moveToFront`

**Status:** Active
**Phase / Commit:** L7h (post-L7g hotfix)
**Files:**
- `crates/gpui_android/src/multi_window.rs` (`activate_extra_activity`)
- `crates/gpui_android/src/window.rs` (`PlatformWindow::activate` impl)

## Problem

When the user has an extra window open (e.g. Settings) but it's in the
background, tapping Zed → Settings → Open Settings again silently no-ops.
Production code's dedup at `settings_ui.rs:622-647` correctly finds the
existing `SettingsWindow` and calls `window.activate_window()`, but on
Android `activate_window()` was a no-op stub — the existing background
Activity stayed exactly where it was, invisible.

Same pattern affects any `cx.windows().find(downcast::<X>)` + `activate_window()`
flow: command palette restoration, recent projects redisplay, etc.

## Constraint

`gpui::Window::activate_window()` calls `platform_window.activate()`
(`gpui/src/window.rs:4856`). The platform impl on Android was the default
empty stub:

```rust
fn activate(&self) {}
```

To actually surface a backgrounded freeform Activity we need a JNI call
that asks the OS to move the Activity's task to the foreground. There are
several options:

| API | Permission | Self-only? |
|---|---|---|
| `ActivityManager.moveTaskToFront(taskId, flags)` | `REORDER_TASKS` (dangerous) | No |
| `Activity.moveTaskToBack()` | none | self only (wrong direction) |
| `ActivityManager.AppTask.moveToFront()` | none | yes — own tasks only |

`AppTask.moveToFront` is the official self-only path. No new permission
required.

## Solution

`AndroidWindow::activate` (only meaningful for extra windows) routes to
`multi_window::activate_extra_activity`, which:

1. Looks up the Activity's `GlobalRef` in `EXTRA_ACTIVITY_REFS` by
   `window_id`. Bails silently if not registered (Activity already
   destroyed).
2. Reads `Activity.getTaskId()` to identify the task.
3. Calls `Activity.getSystemService(Context.ACTIVITY_SERVICE)` to get
   `ActivityManager`.
4. Iterates `ActivityManager.getAppTasks()` (returns the calling app's
   own tasks only, no `GET_TASKS` permission required).
5. For each `AppTask`, reads `getTaskInfo()` and matches the `taskId` /
   `id` field. Calls `AppTask.moveToFront()` on the match.
6. Clears any pending JNI exception before returning.

```rust
// crates/gpui_android/src/multi_window.rs
pub(crate) fn activate_extra_activity(android_app: &AndroidApp, window_id: u64) {
    let result = (|| -> Result<()> {
        // ... attach JVM, look up GlobalRef ...
        let task_id = env.call_method(activity_ref.as_obj(), "getTaskId", "()I", &[])?.i()?;
        let activity_manager = env
            .call_method(activity_ref.as_obj(), "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&env.new_string("activity")?)])?
            .l()?;
        let app_tasks = env.call_method(&activity_manager, "getAppTasks",
            "()Ljava/util/List;", &[])?.l()?;
        // iterate, match taskId, AppTask.moveToFront()
        Ok(())
    })();
    // log on err, exception_clear
}
```

Both `taskId` (API 29+, on `TaskInfo`) and `id` (deprecated since API
29, on `RecentTaskInfo`) field names are tried, since either may be the
populated one depending on the framework version actually loaded. Try
modern first, fall back to legacy.

```rust
// crates/gpui_android/src/window.rs
fn activate(&self) {
    let Some(window_id) = self.extra_window_id else { return };
    let android_app = self.ptr.state.borrow().android_app.clone();
    crate::multi_window::activate_extra_activity(&android_app, window_id);
}
```

## Why this works

`getAppTasks()` + `AppTask.moveToFront()` is the documented
permissionless self-task surfacing API since Android 5.0 (API 21). Each
`ExtraWindowActivity` is in its own freeform task (via
`taskAffinity="com.zdroid.extra"` + `documentLaunchMode="always"`),
so taskId uniquely identifies the windowId-to-Activity mapping.

Verified on device: with Settings extra Activity in foreground, tapping
Zed → Settings → Open Settings while Settings task is in the background
fires `AppTaskImpl.moveToFront:195` in the system WindowManager logs and
brings the existing freeform window forward. NO second `open_extra_window`
spawn, no duplicate Activity, no GlobalRef leak.

## Failure mode if regressed

- Tap Zed → Settings → Open Settings while Settings is in background: nothing
  visible happens, the existing Settings window stays buried.
- User has to swipe-up to Recents and tap the Settings task card to
  surface it manually.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
- [`with_active_or_new_workspace` Android fallback](with-active-or-new-workspace-android-fallback.md) — sibling fix for theme picker / command palette / etc. that route through the workspace
- `settings_ui::open_settings_editor` (`crates/settings_ui/src/settings_ui.rs:622-647`) — the existing-window dedup that drives this path
