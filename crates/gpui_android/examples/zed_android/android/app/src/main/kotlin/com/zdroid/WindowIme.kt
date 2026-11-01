package com.zdroid

import android.app.Activity
import android.util.Log
import android.view.ViewGroup
import android.view.inputmethod.InputMethodManager
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

/// One window's on-screen keyboard: the [ImeHostView] that owns its input
/// connection, whether it's showing, and what the keyboard reads back.
///
/// Whether it's showing comes only from the window's insets, which Android
/// keeps right however the keyboard came or went (Back, a swipe, another app).
/// Rust's status bar button sets its own copy as soon as it asks, so it answers
/// a tap at once; [reportVisibility] corrects that copy when a request does
/// nothing, as a show does when One UI's Samsung Keyboard cancels it (seen with
/// the Book Cover keyboard attached) without the insets changing.
class WindowIme(
    private val activity: Activity,
    override val imeWindowId: Long,
    keyboardAccessoryHeight: () -> Int = { 0 },
    onImeBottomChanged: (Int) -> Unit = {},
    private val onVisibilityChanged: () -> Unit = {},
    private val modifierState: () -> Int = { 0 },
    private val clearPendingModifier: () -> Unit = {},
) : ImeHost {
    val insetsReporter =
        WindowInsetsReporter(imeWindowId, keyboardAccessoryHeight, onImeBottomChanged)

    /// Whether the keyboard is up, as the window's insets last said.
    var visible: Boolean = false
        private set

    private val inputMethodManager by lazy {
        activity.getSystemService(InputMethodManager::class.java)
    }
    private var hostView: ImeHostView? = null
    private val reportVisibility = Runnable {
        // Another window may be the one with a keyboard now.
        if (activity.hasWindowFocus()) NativeBridge.nativeSetSoftKeyboardVisible(visible)
    }

    /// Rust writes this from its own thread; the keyboard reads it on the UI
    /// thread.
    @Volatile
    private var textState: ImeTextState? = null

    @Volatile
    override var currentImeMode: Int = ImeInputMode.CODE_EDITOR
        private set

    override var softKeyboardEnabled: Boolean = false
        private set

    override fun getImeTextState(): ImeTextState? = textState

    override val extraKeysModifierState: Int
        get() = modifierState()

    override fun clearExtrasPendingModifier() = clearPendingModifier()

    /// Adds the host view to the Activity, whose content view must be set.
    fun install() {
        val view = ImeHostView(activity, this)
        activity.addContentView(view, ViewGroup.LayoutParams(1, 1))
        hostView = view
        ViewCompat.setOnApplyWindowInsetsListener(view) { _, insets ->
            setVisible(insets.isVisible(WindowInsetsCompat.Type.ime()))
            insetsReporter.onApplyWindowInsets(insets)
            insets
        }
        insetsReporter.install(view)
    }

    private fun setVisible(shown: Boolean) {
        if (visible == shown) return
        visible = shown
        Log.i(TAG, "w=$imeWindowId keyboard visible=$shown")
        NativeBridge.nativeSetSoftKeyboardVisible(shown)
        onVisibilityChanged()
    }

    fun request(show: Boolean) {
        activity.runOnUiThread {
            val view = hostView ?: return@runOnUiThread
            if (show) {
                if (!visible) {
                    if (!view.isFocused) view.requestFocus()
                    inputMethodManager.showSoftInput(view, 0)
                }
            } else {
                WindowInsetsControllerCompat(activity.window, activity.window.decorView)
                    .hide(WindowInsetsCompat.Type.ime())
            }
            view.removeCallbacks(reportVisibility)
            view.postDelayed(reportVisibility, VISIBILITY_REPORT_DELAY_MS)
        }
    }

    /// Asks the keyboard for a new input connection, so it reads the new
    /// target's `EditorInfo` and drops a composition from the old one.
    fun restartForTarget(modeId: Int) {
        activity.runOnUiThread {
            if (currentImeMode == modeId) return@runOnUiThread
            currentImeMode = modeId
            hostView?.let { inputMethodManager.restartInput(it) }
        }
    }

    /// Without this update, keyboards re-send a composition to check where the
    /// cursor went, which typed duplicate letters.
    fun updateTextState(state: ImeTextState) {
        textState = state
        activity.runOnUiThread {
            hostView?.let { view ->
                inputMethodManager.updateSelection(
                    view,
                    state.selectionStart,
                    state.selectionEnd,
                    state.composingStart,
                    state.composingEnd,
                )
            }
        }
    }

    fun setSoftKeyboardEnabled(enabled: Boolean) {
        activity.runOnUiThread { softKeyboardEnabled = enabled }
    }

    private companion object {
        const val TAG = "zdroid_ime"

        /// Long enough for a slow keyboard to have started showing.
        const val VISIBILITY_REPORT_DELAY_MS = 1200L
    }
}
