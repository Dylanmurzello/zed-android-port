package com.zdroid

import android.content.Context
import android.util.Log
import android.view.View
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection

/// Focusable, invisible 1x1 view that owns the IME's `InputConnection`. The
/// editor draws in a `SurfaceView`, which can't provide one. Touch stays on the
/// surface; keyboard focus comes here, which Android keeps independent of touch.
class ImeHostView(context: Context, private val host: ImeHost) : View(context) {
    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    /// Whether Android may bring up the keyboard for this view.
    /// `Configuration.keyboard` can't say: it stays at `NOKEYS` on tablets with
    /// a Bluetooth keyboard paired, so this asks [PhysicalKeyboard].
    private val keyboardAllowed: Boolean
        get() = host.softKeyboardEnabled && !PhysicalKeyboard.blocksIme

    override fun onCheckIsTextEditor(): Boolean = keyboardAllowed

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        // `onCheckIsTextEditor` only stops one way the keyboard shows: Android
        // still asks for a connection on focus and binds the keyboard if it
        // gets one.
        if (!keyboardAllowed) return null
        val mode = host.currentImeMode
        Log.i("zdroid_ime", "ImeHostView.onCreateInputConnection mode=$mode")
        // VISIBLE_PASSWORD is the only flag Gboard, SwiftKey and Samsung's
        // keyboard all honor to refuse composition and prediction; Samsung's
        // Gboard ignores NO_SUGGESTIONS alone and glide-types over code. Each
        // key then commits directly. It also turns off CJK composition. The
        // editor adds MULTI_LINE so Enter inserts a newline.
        outAttrs.inputType = EditorInfo.TYPE_CLASS_TEXT or
            EditorInfo.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or
            EditorInfo.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
            if (mode == ImeInputMode.TERMINAL) 0 else EditorInfo.TYPE_TEXT_FLAG_MULTI_LINE
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        return ZdroidInputConnection(this, host)
    }
}
