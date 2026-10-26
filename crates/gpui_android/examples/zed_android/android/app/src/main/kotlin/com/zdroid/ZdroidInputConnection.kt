package com.zdroid

import android.util.Log
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.ExtractedText
import android.view.inputmethod.ExtractedTextRequest

private const val TAG = "zdroid_ime"

// Off unless enabled with `adb shell setprop log.tag.zdroid_ime DEBUG`: the
// messages include typed text, which can be a password.
private inline fun debugLog(message: () -> String) {
    if (Log.isLoggable(TAG, Log.DEBUG)) Log.d(TAG, message())
}

/// Bridge from Android's IME (Gboard, Swiftkey, etc.) into gpui's
/// `PlatformInputHandler`. The IME calls methods here for every text
/// edit it wants to perform; we forward each into Rust via
/// `NativeBridge`, and Rust dispatches into the focused window's
/// `PlatformInputHandler` (the same trait macOS NSTextInputClient
/// and Linux text-input-v3 wire to).
///
/// Composition (CJK / prediction / gesture typing) uses
/// `setComposingText` / `finishComposingText`. Plain commits (final
/// keystrokes, voice input) use `commitText`. Backspace / delete uses
/// `deleteSurroundingText`. Hardware-style key events that the IME
/// wants to deliver (Enter, arrows) flow through `sendKeyEvent` and
/// land on the existing `events/keyboard.rs` translator.
///
/// Returns `true` from each method to signal the IME we consumed the
/// event. Returning `false` would cause the IME to fall back to
/// posting the text as KeyEvents, which we explicitly don't want
/// (loses composition info).
class ZdroidInputConnection(private val hostView: View) : BaseInputConnection(hostView, /* fullEditor = */ true) {

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val s = text?.toString() ?: ""
        // Modifier intercept: when the user has armed (or latched)
        // a sticky modifier on `ExtraKeysView` and the next event
        // is a single-character soft-keyboard commit, re-synthesize
        // the commit as a `KeyEvent` carrying the modifier in its
        // metaState. Without this, Gboard's `commitText("c")` lands
        // at the editor as a literal 'c' insert, because Gboard's
        // commit path has no slot for `META_CTRL_*` bits.
        //
        // The lookup uses Android's [KeyCharacterMap.VIRTUAL_KEYBOARD]
        // which maps printable chars to their keyCode + the meta
        // bits needed to type them (e.g. 'C' returns
        // KEYCODE_C / META_SHIFT_ON). We OR our modifier on top so
        // Ctrl+Shift+C goes through correctly when both are active.
        // Multi-character commits skip the intercept and fall
        // through to the plain text path; CJK composition + paste
        // shouldn't be re-keyed.
        val host = hostView.context as? ImeHost
        val modifier = host?.extraKeysModifierState ?: 0
        if (modifier != 0 && s.length == 1) {
            val keyMap = android.view.KeyCharacterMap
                .load(android.view.KeyCharacterMap.VIRTUAL_KEYBOARD)
            val events = keyMap.getEvents(charArrayOf(s[0]))
            val firstDown = events?.firstOrNull { it.action == KeyEvent.ACTION_DOWN }
            if (firstDown != null && firstDown.keyCode != KeyEvent.KEYCODE_UNKNOWN) {
                val combinedMeta = modifier or firstDown.metaState
                debugLog {
                    "IC.commitText w=$windowId intercepted ${quote(s)} as " +
                        "key=${firstDown.keyCode} meta=0x${Integer.toHexString(combinedMeta)}"
                }
                NativeBridge.nativeImeSendKeyEvent(
                    windowId,
                    KeyEvent.ACTION_DOWN,
                    firstDown.keyCode,
                    combinedMeta,
                    0,
                )
                NativeBridge.nativeImeSendKeyEvent(
                    windowId,
                    KeyEvent.ACTION_UP,
                    firstDown.keyCode,
                    combinedMeta,
                    0,
                )
                host?.clearExtrasPendingModifier()
                return true
            }
        }
        // Vim command-mode routing: when the focused editor is in a
        // vim command mode the committed text has to reach the editor
        // as key *events* so vim's keymap reads it as motions /
        // operators (`j`, `d`, `w`) instead of inserting the literal
        // characters. We re-key the whole commit through
        // `KeyCharacterMap` (same mechanism as the modifier intercept
        // above) so the full down/up sequence carries the right keyCode
        // and metaState — shifted letters (`G`, `A`) and symbols (`$`,
        // `:`, `/`) come through as the keystrokes vim expects. If any
        // char has no virtual-keyboard mapping `getEvents` returns null
        // for the batch; we fall through to a plain commit so nothing is
        // silently dropped (e.g. an emoji pasted in normal mode).
        if (s.isNotEmpty() && NativeBridge.nativeImeRouteAsKeys()) {
            val keyMap = android.view.KeyCharacterMap
                .load(android.view.KeyCharacterMap.VIRTUAL_KEYBOARD)
            val events = keyMap.getEvents(s.toCharArray())
            if (events != null && events.isNotEmpty()) {
                debugLog { "IC.commitText w=$windowId vim-route ${quote(s)} as ${events.size} key events" }
                for (ev in events) {
                    NativeBridge.nativeImeSendKeyEvent(
                        windowId,
                        ev.action,
                        ev.keyCode,
                        ev.metaState,
                        ev.repeatCount,
                    )
                }
                return true
            }
            debugLog { "IC.commitText w=$windowId vim-route fallthrough (no keymap) ${quote(s)}" }
        }
        debugLog { "IC.commitText w=$windowId text=${quote(s)} cursor=$newCursorPosition" }
        NativeBridge.nativeImeCommitText(windowId, s, newCursorPosition)
        return true
    }

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val s = text?.toString() ?: ""
        debugLog { "IC.setComposingText w=$windowId text=${quote(s)} cursor=$newCursorPosition" }
        NativeBridge.nativeImeSetComposingText(windowId, s, newCursorPosition)
        return true
    }

    override fun finishComposingText(): Boolean {
        debugLog { "IC.finishComposingText w=$windowId" }
        NativeBridge.nativeImeFinishComposingText(windowId)
        return true
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        debugLog { "IC.deleteSurroundingText w=$windowId before=$beforeLength after=$afterLength" }
        NativeBridge.nativeImeDeleteSurroundingText(windowId, beforeLength, afterLength)
        return true
    }

    override fun sendKeyEvent(event: KeyEvent?): Boolean {
        event ?: return false
        // Fold in any armed ExtraKeysView modifier (Ctrl / Alt / Shift)
        // on keys the IME delivers through this path. The important one
        // is Enter (keyCode 66), which Samsung's keyboard routes here, so
        // an armed Shift yields Shift+Enter (e.g. a newline in Claude
        // Code's prompt), Ctrl yields Ctrl+Enter, and so on. The
        // commitText intercept only covers single-character commits, not
        // these hardware-style keys, so Enter / Backspace / arrows
        // arriving here would otherwise drop the modifier. The OR is
        // idempotent if the keyboard already set the same bit. Pending
        // (one-shot) modifiers clear on the key's UP so the modifier
        // spans the whole down/up pair; a latched modifier stays until
        // the user unlatches it on the row.
        val host = hostView.context as? ImeHost
        val modifier = host?.extraKeysModifierState ?: 0
        val meta = event.metaState or modifier
        debugLog {
            "IC.sendKeyEvent w=$windowId action=${event.action} keyCode=${event.keyCode} " +
                "meta=0x${Integer.toHexString(meta)} (extras=0x${Integer.toHexString(modifier)}) " +
                "repeat=${event.repeatCount} unicode=${event.unicodeChar} chars=${quote(event.characters ?: "")}"
        }
        NativeBridge.nativeImeSendKeyEvent(
            windowId,
            event.action,
            event.keyCode,
            meta,
            event.repeatCount,
        )
        if (modifier != 0 && event.action == KeyEvent.ACTION_UP) {
            host?.clearExtrasPendingModifier()
        }
        return true
    }

    override fun performEditorAction(actionCode: Int): Boolean {
        debugLog { "IC.performEditorAction w=$windowId action=$actionCode" }
        NativeBridge.nativeImePerformEditorAction(windowId, actionCode)
        return true
    }

    // ---- Read path ----
    // The IME queries these to know the current text + selection
    // state so it can refine predictions, position candidates, etc.
    // Without these, Gboard's state-sync model can't trust its
    // internal view, and it re-sends commits / compositions
    // defensively (the ~155ms duplicate setComposingText pattern we
    // saw in the log). We answer from MainActivity's
    // `ImeTextState` mirror, which Rust pushes via JNI on every text
    // change.

    private fun mirror(): ImeTextState =
        (hostView.context as? ImeHost)?.getImeTextState() ?: ImeTextState.EMPTY

    /// Identifier of the gpui window this host's input flows
    /// into. Passed through to every `nativeIme*` JNI call so Rust
    /// can route the event to the right window's
    /// `PlatformInputHandler`. `0` = primary (MainActivity).
    private val windowId: Long = (hostView.context as? ImeHost)?.imeWindowId ?: 0L

    override fun getTextBeforeCursor(n: Int, flags: Int): CharSequence? {
        val text = mirror().textBeforeCursor(n)
        debugLog { "IC.getTextBeforeCursor n=$n -> len=${text.length}" }
        return text
    }

    override fun getTextAfterCursor(n: Int, flags: Int): CharSequence? {
        val text = mirror().textAfterCursor(n)
        debugLog { "IC.getTextAfterCursor n=$n -> len=${text.length}" }
        return text
    }

    override fun getSelectedText(flags: Int): CharSequence? {
        val text = mirror().selectedText()
        if (text.isEmpty()) return null
        debugLog { "IC.getSelectedText -> len=${text.length}" }
        return text
    }

    override fun getExtractedText(request: ExtractedTextRequest?, flags: Int): ExtractedText? {
        val state = mirror()
        debugLog {
            "IC.getExtractedText -> len=${state.text.length} sel=${state.selectionStart}..${state.selectionEnd}"
        }
        return state.extractedText()
    }

    private fun quote(s: String): String =
        "\"" + s.replace("\\", "\\\\").replace("\"", "\\\"") + "\""
}
