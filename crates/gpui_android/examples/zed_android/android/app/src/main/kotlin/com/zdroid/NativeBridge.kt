package com.zdroid

import android.view.Surface

/// Single source of truth for all JNI declarations bridging Kotlin → Rust
/// for multi-window surface lifecycle and touch dispatch. Both
/// `MainActivity` and `ExtraWindowActivity` call into this object so the
/// JNI symbol set lives in one place — adding a new bridge fn means a single
/// declaration here and a single matching `Java_..._NativeBridge_*` extern in
/// `crates/gpui_android/src/multi_window.rs`.
///
/// **JNI symbol mangling:** these `external fun`s resolve to
/// `Java_com_zdroid_NativeBridge_<methodName>` symbols. Class name
/// changes here require matching renames on the Rust side.
object NativeBridge {
    /// Process-death recovery probe. `ExtraWindowActivity.onCreate` calls
    /// this BEFORE any other JNI work. Returns true if the gpui-side has a
    /// live AndroidWindow registered for this `windowId` (this Activity was
    /// launched in the current Rust process and gpui knows about it).
    /// Returns false on resurrection from Recents after a process kill —
    /// gpui has been re-init'd and has no record of the windowId. Activity
    /// uses the result to either proceed or `finish()` itself.
    external fun nativeIsExtraWindowKnown(windowId: Long): Boolean

    /// Posted from `ExtraWindowActivity.onCreate`. Rust stores a `GlobalRef`
    /// to the activity in its registry so it can later issue
    /// `finishAndRemoveTask` for gpui-initiated close. Must fire BEFORE
    /// `nativeOnExtraSurfaceCreated` (the SurfaceHolder.Callback may not
    /// have run yet at this point — that's fine, ordering is enforced by
    /// the Activity lifecycle).
    external fun nativeOnExtraActivityCreated(windowId: Long, activity: Any)

    /// Posted from `ExtraWindowActivity.onDestroy`. Removes the GlobalRef
    /// from the registry and posts an `OsClosed` event so the gpui-side
    /// `Window::remove_window()` flow runs.
    external fun nativeOnExtraActivityDestroyed(windowId: Long)

    /// Posted by a `SurfaceHolder.Callback` when the surface is first ready.
    /// Rust unwraps the Surface into an `ANativeWindow` and either resolves
    /// the pending `oneshot` (first attach) or routes through the event
    /// channel (re-attach after Activity recreation).
    external fun nativeOnExtraSurfaceCreated(windowId: Long, surface: Surface)

    /// Posted by `surfaceChanged`. Width/height drive the Vulkan swapchain
    /// reconfigure on the Rust side.
    external fun nativeOnExtraSurfaceChanged(
        windowId: Long,
        surface: Surface,
        format: Int,
        width: Int,
        height: Int,
    )

    /// Posted by `surfaceDestroyed` — Rust must stop submitting frames
    /// synchronously inside this callback.
    external fun nativeOnExtraSurfaceDestroyed(windowId: Long)

    /// Posted from `ExtraWindowActivity.onWindowFocusChanged`, so gpui knows
    /// which window is active. Zed closes the active window for Ctrl+W.
    external fun nativeOnExtraFocusChanged(windowId: Long, hasFocus: Boolean)

    /// Posted by [WindowInsetsReporter] for `MainActivity`
    /// (`MainActivity.PRIMARY_WINDOW_ID`) and each extra window, in physical
    /// pixels: the `systemBars() | displayCutout()` insets, and how far the
    /// on-screen keyboard and the programming extras row on it reach up
    /// from the window's bottom edge. Rust lays gpui out above them.
    /// Handled in `crates/gpui_android/src/insets.rs`.
    external fun nativeOnWindowInsetsChanged(
        windowId: Long,
        safeAreaLeft: Int,
        safeAreaTop: Int,
        safeAreaRight: Int,
        safeAreaBottom: Int,
        obscuredBottom: Int,
    )

    /// MotionEvent fields marshaled into primitive arrays (we can't share
    /// `MotionEvent` across the JNI boundary). Pointer indices are
    /// `0..pointerCount-1`. `vscroll`/`hscroll` carry the
    /// `MotionEvent.AXIS_VSCROLL`/`AXIS_HSCROLL` values for `ACTION_SCROLL`
    /// events (mouse wheel, trackpad two-finger scroll); zero on touch /
    /// hover / button events. `flags` is `MotionEvent.getFlags()` and
    /// `toolTypes` each pointer's `MotionEvent.getToolType`.
    external fun nativeOnExtraTouchEvent(
        windowId: Long,
        actionMasked: Int,
        actionIndex: Int,
        metaState: Int,
        buttonState: Int,
        flags: Int,
        eventTimeMillis: Long,
        vscroll: Float,
        hscroll: Float,
        xs: FloatArray,
        ys: FloatArray,
        pointerIds: IntArray,
        toolTypes: IntArray,
    )

    /// Hardware key event forwarder. Called from `ExtraWindowActivity.
    /// dispatchKeyEvent` so editor focus inside an extra window (Settings,
    /// command palette in a detached window, etc.) actually receives
    /// keystrokes. Without this, the gpui-side `set_input_handler` is
    /// registered but no `PlatformInput::KeyDown` ever fires — the editor
    /// has focus but typing is a no-op.
    ///
    /// `action`: `KeyEvent.ACTION_DOWN` / `ACTION_UP`.
    /// `keyCode`: AKEYCODE_*.
    /// `metaState`: META_* bitfield (shift/ctrl/alt/meta/caps lock state).
    /// `repeatCount`: 0 for the initial press, >0 for auto-repeat.
    external fun nativeOnExtraKeyEvent(
        windowId: Long,
        action: Int,
        keyCode: Int,
        metaState: Int,
        repeatCount: Int,
    )

    /// True while a hold-and-drag gesture is in flight on the given
    /// window's Rust-side state machine. Kotlin queries on every
    /// multi-touch MOVE event to decide whether the on-screen cursor
    /// sprite should follow the moving finger. Per-window because
    /// each spawned `ExtraWindowActivity` has its own gesture state;
    /// pass `0` for `MainActivity` (the primary window) and the
    /// extra window's id for spawned windows.
    external fun isHoldDragActive(windowId: Long): Boolean

    /// Polled every frame by the SplashScreen's
    /// `setKeepOnScreenCondition`. Returns true once gpui has run at
    /// least one paint cycle; the splash then exits with its
    /// registered animation and the activity switches to
    /// `Theme.Zdroid.Main`. The Rust side flips the atomic backing
    /// this in `platform.rs` immediately after the first
    /// `frame_timing::paint_finished` call.
    external fun nativeIsZedReady(): Boolean

    /// Captured-pointer sink, called only by [forwardCapturedPointer].
    /// `windowId` is `MainActivity.PRIMARY_WINDOW_ID` or the extra
    /// window's id. `xs`/`ys` are absolute pointer positions in the
    /// touchpad's coordinate space; `rxs`/`rys` are `AXIS_RELATIVE_X` /
    /// `AXIS_RELATIVE_Y` per pointer (the deltas that drive cursor
    /// motion and scroll synthesis). `vscroll`/`hscroll` carry a
    /// mouse wheel's `ACTION_SCROLL`.
    external fun nativeOnCapturedPointer(
        windowId: Long,
        actionMasked: Int,
        source: Int,
        buttonState: Int,
        metaState: Int,
        pointerCount: Int,
        xs: FloatArray,
        ys: FloatArray,
        rxs: FloatArray,
        rys: FloatArray,
        vscroll: Float,
        hscroll: Float,
        cursorPhysicalX: Float,
        cursorPhysicalY: Float,
    )

    /// IME `commitText` from `ZdroidInputConnection`. Final text the
    /// IME wants inserted at the current cursor (or replacing any
    /// active composition). The `windowId` selects which gpui
    /// window's `PlatformInputHandler` receives the edit — `0` for
    /// the primary `MainActivity` surface, the value gpui assigned
    /// to spawned `ExtraWindowActivity` instances for those.
    external fun nativeImeCommitText(windowId: Long, text: String, newCursorPosition: Int)

    /// IME `setComposingText`. In-progress composition (CJK, gesture
    /// typing, prediction). Routes to `replace_and_mark_text_in_range`
    /// on the window identified by `windowId`.
    external fun nativeImeSetComposingText(windowId: Long, text: String, newCursorPosition: Int)

    /// IME `finishComposingText`. End of composition without further
    /// edits. Routes to `unmark_text` on the target window.
    external fun nativeImeFinishComposingText(windowId: Long)

    /// IME `deleteSurroundingText(before, after)`. Backspace / delete
    /// spans a range around the cursor of the target window.
    external fun nativeImeDeleteSurroundingText(
        windowId: Long,
        beforeLength: Int,
        afterLength: Int,
    )

    /// IME `sendKeyEvent` fallback for hardware-style key events the
    /// IME wants to deliver (Enter, Backspace, arrows). Routed to
    /// the target window's `handle_input` via
    /// `events/keyboard.rs::translate_extra_key_event`.
    external fun nativeImeSendKeyEvent(
        windowId: Long,
        action: Int,
        keyCode: Int,
        metaState: Int,
        repeatCount: Int,
    )

    /// IME `performEditorAction(actionId)` — Enter / Done / Next /
    /// Search etc. Emits a gpui action on the focused element of the
    /// target window.
    external fun nativeImePerformEditorAction(windowId: Long, actionId: Int)

    /// Whether soft-keyboard text for this window has to be re-delivered
    /// as key events rather than inserted as literal text: true when its
    /// focused field doesn't accept typed text, as gpui reports, such as
    /// an editor in vim's normal or visual mode (so vim reads `j`/`d`/`w`
    /// as motions and operators).
    external fun nativeImeRouteAsKeys(windowId: Long): Boolean

    /// Mirror Kotlin's `imeShown` flag into the Rust-side global
    /// `SOFT_KEYBOARD_VISIBLE` atomic. Pushed on every transition
    /// (showSoftInput / hideSoftInputFromWindow / WindowInsetsListener
    /// edge). The status bar keyboard button reads this via
    /// `gpui_android::soft_keyboard_visible()` to render its lit-up
    /// `toggle_state`.
    external fun nativeSetSoftKeyboardVisible(visible: Boolean)

    /// Posted by `MainActivity` at startup and whenever an input device or
    /// Android's "show on-screen keyboard with a physical keyboard" setting
    /// changes: [PhysicalKeyboard.blocksIme]. Read through
    /// `gpui_android::physical_keyboard_blocks_ime()`.
    external fun nativeSetPhysicalKeyboardBlocksIme(blocksIme: Boolean)

    /// Posted by [UpdateInstallReceiver] when an update session ends
    /// without installing: cancelled in Android's prompt or failed.
    external fun nativeOnUpdateInstallResult(status: Int, message: String?)

    /// Posted when the user returns from Zdroid's "Install unknown apps"
    /// settings page, which [MainActivity.requestInstallPermission] opens.
    external fun nativeOnInstallPermissionResult(granted: Boolean)
}
