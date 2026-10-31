package com.zdroid

import android.content.Context
import android.view.View
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

/// Focusable invisible view that hosts the IME `InputConnection`.
/// GameActivity's `SurfaceView` renders the editor and receives touch
/// input via the NDK native input queue, but it doesn't (and can't
/// easily be subclassed to) provide an `InputConnection` of our
/// choosing. This view sits alongside the `SurfaceView` in the
/// Activity's content hierarchy at 1×1 px (effectively invisible) and
/// is the focus target whenever the editor wants the soft keyboard.
///
/// Focus is requested explicitly from `MainActivity` when Rust signals
/// "this window now has text input focus" (via `set_input_handler`).
/// Touch events stay on the `SurfaceView`; keyboard focus comes here
/// without affecting touch dispatch (the two are independent in
/// Android's input model).
class ImeHostView(context: Context) : View(context) {
    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    /// Returning `true` here is what makes Android auto-show the
    /// IME the moment this view gains focus — same path EditText
    /// uses. Termux's TerminalView is a plain `View` that doesn't
    /// override this method, which is why Termux never shows Gboard
    /// on focus.
    ///
    /// We gate on:
    ///   1. The user's `android_input.on_screen_keyboard` setting (the
    ///      Activity's [ImeHost.softKeyboardEnabled], pushed by Rust's
    ///      reconcile tick).
    ///   2. A connected physical keyboard that blocks the on-screen one,
    ///      [PhysicalKeyboard.blocksIme]. `Configuration.keyboard` is
    ///      unreliable — it reports the *built-in* physical keyboard and
    ///      stays at `NOKEYS` for tablets even when a BT keyboard is
    ///      paired and producing key events. `InputManager.inputDeviceIds`
    ///      + each device's `keyboardType` is the source of truth.
    override fun onCheckIsTextEditor(): Boolean {
        val userWants = (context as? ImeHost)?.softKeyboardEnabled ?: false
        return userWants && !PhysicalKeyboard.blocksIme
    }

    companion object {
        /// How long after a show request [checkImeShownLater] looks, and
        /// how long it waits before looking again.
        private const val IME_SHOW_CHECK_DELAY_MS = 600L
    }

    /// Calls [ifNotVisible] if the IME isn't visible a moment after a show
    /// request, and still isn't a moment later. One UI's Samsung Keyboard
    /// can cancel a show right away (seen with the Book Cover keyboard
    /// attached) without the IME inset ever changing, so the Activity's
    /// insets listener never learns of it; the second look keeps a slow
    /// keyboard from being taken for a cancelled one.
    fun checkImeShownLater(ifNotVisible: () -> Unit) {
        checkImeShownLater(checksLeft = 2, ifNotVisible)
    }

    private fun checkImeShownLater(checksLeft: Int, ifNotVisible: () -> Unit) {
        postDelayed({
            val insets = ViewCompat.getRootWindowInsets(this) ?: return@postDelayed
            if (insets.isVisible(WindowInsetsCompat.Type.ime())) return@postDelayed
            if (checksLeft > 1) {
                checkImeShownLater(checksLeft - 1, ifNotVisible)
            } else {
                ifNotVisible()
            }
        }, IME_SHOW_CHECK_DELAY_MS)
    }

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        // Hard kill switch. `onCheckIsTextEditor()` returning false
        // only suppresses ONE auto-show path; Android still calls
        // `onCreateInputConnection` on focus and binds the IME if we
        // return a non-null connection. Returning null here aborts
        // the bind entirely — no Gboard, no inset, no nothing. The
        // user's manual `imm.showSoftInput` call from
        // `MainActivity.requestIme` (when the setting is on and no HW
        // keyboard is connected) lands on the next call to this
        // method, at which point both gates are satisfied and we
        // return a real connection.
        val userWants = (context as? ImeHost)?.softKeyboardEnabled ?: false
        val hwKeyboardBlocksIme = PhysicalKeyboard.blocksIme
        if (!userWants || hwKeyboardBlocksIme) {
            android.util.Log.i(
                "zdroid_ime",
                "ImeHostView.onCreateInputConnection -> null " +
                    "(userWants=$userWants, hwKeyboardBlocksIme=$hwKeyboardBlocksIme)",
            )
            return null
        }
        val mode = (context as? ImeHost)?.currentImeMode ?: ImeInputMode.CODE_EDITOR
        android.util.Log.i("zdroid_ime", "ImeHostView.onCreateInputConnection mode=$mode")
        // EditorInfo per target kind (see ImeInputMode + ImeTargetKind):
        //
        // - TERMINAL: Termux's TerminalView.java line 280 pattern.
        //   TYPE_TEXT_VARIATION_VISIBLE_PASSWORD disables composition
        //   AND prediction in Gboard / Swiftkey. Means each keystroke
        //   commits directly to the PTY (no setComposingText
        //   accumulation, no autocorrect dump-and-retry).
        //
        // - CODE_EDITOR: NO_SUGGESTIONS kills the autocorrect strip
        //   over code tokens, but we deliberately DON'T set
        //   VISIBLE_PASSWORD — that would also disable CJK
        //   composition, which we want to keep working for users
        //   writing comments / strings in their native script.
        //   IME_MULTI_LINE so Enter inserts a newline instead of
        //   triggering "Done".
        // Both modes use VISIBLE_PASSWORD: Samsung's Gboard ignores
        // NO_SUGGESTIONS alone (documented behavior — its prediction
        // strip + glide-typing still fire on code tokens, producing
        // the "lililimeline" autocompletion regression where Gboard
        // turns a sequence of "lili" typings into a long suggested
        // word and commits the whole thing). VISIBLE_PASSWORD is the
        // only flag that reliably refuses composition + prediction
        // across Gboard / Swiftkey / Samsung IME. Trade-off: CJK
        // composition is also disabled in CODE_EDITOR — we add a
        // separate RICH_TEXT mode later if a user needs CJK input
        // (rare in code; common in comments / docs).
        outAttrs.inputType = when (mode) {
            ImeInputMode.TERMINAL ->
                EditorInfo.TYPE_CLASS_TEXT or
                    EditorInfo.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or
                    EditorInfo.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            else /* CODE_EDITOR */ ->
                EditorInfo.TYPE_CLASS_TEXT or
                    EditorInfo.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or
                    EditorInfo.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
                    EditorInfo.TYPE_TEXT_FLAG_MULTI_LINE
        }
        outAttrs.imeOptions =
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
                EditorInfo.IME_FLAG_NO_EXTRACT_UI or
                EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        return ZdroidInputConnection(this)
    }

}
