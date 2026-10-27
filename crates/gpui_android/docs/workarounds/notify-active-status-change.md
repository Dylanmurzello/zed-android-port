# Fire `on_active_status_change` after extra-window attach

**Status:** Active
**Phase / Commit:** L7 (post-shipping fix)
**Files:**
- `crates/gpui_android/src/window.rs` (`notify_active_status_change`, `set_active`)
- `crates/gpui_android/src/platform.rs` (`open_extra_window` deferred spawn, `window_focus_changed`)
- `crates/gpui_android/src/multi_window.rs` (`nativeOnExtraFocusChanged`)
- `ExtraWindowActivity.onWindowFocusChanged` in the Android app

## Problem

In the multi-Activity Settings window, the search field's text cursor
rendered **statically** on first open — visible but not blinking. After any
input event (tapping the search bar, typing), the cursor began blinking
normally and continued to behave correctly.

Production Zed on macOS/Linux: cursor blinks immediately on first paint.

## Constraint

The editor's cursor blink is gated on `BlinkManager::enabled`. `enable()`
gets called from two paths:

1. **`Editor::handle_focus`** — fires when the editor's `FocusHandle`
   becomes the window's focused handle (line `editor.rs:25994`).
2. **`cx.observe_window_activation`** — fires when the window's `active`
   state changes via the platform's `on_active_status_change` callback
   (line `editor.rs:2618`).

Settings UI explicitly focuses the search bar at construction
(`editor.focus_handle(cx).focus(window, cx)` at `settings_ui.rs:1696`),
so path (1) should fire. Empirically, **it does NOT** — instrumented
`BlinkManager::enable` never logs for the extra-window editor. Path (2)
is gpui's only other route, and it requires the platform to call
`on_active_status_change` after the gpui Window registers its callback.

We never invoked that callback for any window — primary GameActivity got
away with it because the welcome page has no focused text input that
visibly cares; Settings exposed the bug.

Critical timing detail: gpui registers the platform callback inside
`Window::new`, which runs AFTER our `Platform::open_window` returns.
So we can't fire it synchronously from `open_extra_window` — the slot is
empty at that point.

## Solution

Add a `notify_active_status_change(active)` method on
`AndroidWindowStatePtr` that invokes the registered callback if present,
and schedule a **deferred** call via the foreground executor in
`open_extra_window` so it lands on a tick AFTER gpui's `Window::new` has
finished registering callbacks:

```rust
// In AndroidWindowStatePtr (window.rs):
pub(crate) fn notify_active_status_change(&self, active: bool) {
    let callback = self.callbacks.borrow_mut().active_status_change.take();
    if let Some(mut callback) = callback {
        callback(active);
        self.callbacks.borrow_mut().active_status_change = Some(callback);
    }
}

// In open_extra_window (platform.rs), at the end:
let executor = self.common.borrow().foreground_executor.clone();
let window_ptr = window.ptr();
executor
    .spawn(async move {
        let active = window_ptr.state.borrow().active;
        window_ptr.notify_active_status_change(active);
    })
    .detach();
```

The deferred spawn runs on the next foreground tick, after gpui's
`Window::new` has wired all callbacks. The callback fires
`window.activation_observers`, which the editor registered via
`cx.observe_window_activation`. The observer reads
`window.is_window_active()` (true) and calls `BlinkManager::enable`. Blink
loop starts. Cursor animates.

## Why this works

Verified end-to-end on device. Instrumented `BlinkManager::enable`
confirms: before the fix, `enable()` was never called for the Settings
search bar editor. After the fix, `enable() called, was_enabled=false`
fires within ~500ms of window open, and the blink loop runs cleanly with
epoch counter incrementing every 500ms.

Without the deferred spawn (firing synchronously inside
`open_extra_window`): `state.callbacks.active_status_change` is `None`
because gpui hasn't run `Window::new` yet — the callback is registered at
gpui's `window.rs:1455`, which is part of the post-`open_window` Window
construction flow.

## Failure mode if regressed

- Settings search cursor renders statically (visible but not blinking)
  on first open. After any input the bug "self-heals" because
  `Editor::handle_input → update_selections → pause_blinking` triggers
  the blink loop manually — but only after `enable()` has run, which it
  hadn't until our notify path.
- Removing the `executor.spawn(...)` and firing synchronously: `enable()`
  never runs because the callback slot is empty at our call site.
- Forgetting to fire entirely (revert): same as the original bug.

## Focus

`is_active()` used to return true for every window, and the deferred spawn
always sent `true`. Zed's handler for `workspace::CloseWindow`
(`Workspace::close_global`) closes the first window that reports active,
which was always the main window. Ctrl+W or Ctrl+Shift+W in Settings closed
the main window instead of Settings, and Zdroid quit. One UI's close button
on the pop-up wasn't affected: it finishes the Activity, and the `OsClosed`
event closes that window directly.

Each window now reports its Activity's input focus, as Zed's Linux and
Windows backends report keyboard focus:

- the main window from GameActivity's `MainEvent::GainedFocus` and
  `MainEvent::LostFocus`;
- extra windows from `ExtraWindowActivity.onWindowFocusChanged`, through
  `nativeOnExtraFocusChanged` and `ExtraWindowEvent::FocusChanged`.

A window starts active because it opens in front, and the deferred spawn
sends the current state because focus may have changed before it runs.
Gaining focus also makes the window the platform's `active_window`. Losing
focus keeps it, because focus also moves to system UI such as the
notification shade, and Zed opens things in the active window.

Zed now hides the text cursor, and stops other activation-gated work such
as animated images, in windows that don't have focus.

## See also

- [Multi-Activity OS-chromed extra windows](multi-activity-os-chrome.md)
- `editor::blink_manager::BlinkManager` — the blink state machine
- `gpui::Window::new` (`gpui/src/window.rs:1455`) — where the callback is registered
- `gpui::App::observe_window_activation` (`gpui/src/app/context.rs:444`) — observer registration
