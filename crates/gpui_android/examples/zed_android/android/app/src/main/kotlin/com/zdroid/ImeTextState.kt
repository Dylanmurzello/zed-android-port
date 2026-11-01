package com.zdroid

import android.text.SpannableStringBuilder
import android.view.inputmethod.ExtractedText

/// Kotlin-side mirror of the gpui editor's text state that the IME
/// can query synchronously. Updated by Rust after every commit /
/// compose / delete via `MainActivity.updateImeTextState`.
///
/// We mirror only a window of text around the cursor (~256 chars
/// each side) rather than the full document, because the IME never
/// needs more than that for prediction / context, and a Zed buffer
/// can be megabytes large.
///
/// All indices are UTF-16 code-unit offsets in the FULL document,
/// not in `text`. Use `relInWindow()` to convert an absolute index to
/// a position within `text` for substring extraction.
data class ImeTextState(
    val text: String,
    val windowStart: Int,
    val selectionStart: Int,
    val selectionEnd: Int,
    val composingStart: Int, // -1 = no active composition
    val composingEnd: Int,   // -1 = no active composition
) {
    /// Convert an absolute UTF-16 offset to an index within `text`.
    /// Returns null if the offset falls outside the mirrored window
    /// (caller should report the empty string or fall back gracefully).
    private fun relInWindow(absolute: Int): Int? {
        val rel = absolute - windowStart
        return if (rel in 0..text.length) rel else null
    }

    /// `InputConnection.getTextBeforeCursor(n)` — n UTF-16 units of
    /// text BEFORE the cursor that is NOT part of the active
    /// composition. Per Android contract (InputConnection
    /// Javadoc): "This will not include any currently composing
    /// text".
    ///
    /// Subtle: when composition is active, the buffer already
    /// contains the composing letters (replace_and_mark inserted
    /// them with a marked highlight). Cursor sits at composingEnd.
    /// If we naively returned text up to selectionStart we'd hand
    /// Gboard the composing letters as part of the prior context —
    /// Gboard then treats them as "the word the user is currently
    /// typing" plus surrounding buffer, and on the next keystroke
    /// rebuilds a `setComposingText` that includes the surrounding
    /// buffer too. That's the "editor vomits a garbage paste"
    /// regression. Truncating at composingStart fixes it.
    fun textBeforeCursor(n: Int): CharSequence {
        val boundary = if (composingStart in 0 until selectionStart) composingStart else selectionStart
        val end = relInWindow(boundary) ?: return ""
        val start = (end - n).coerceAtLeast(0)
        return text.substring(start, end)
    }

    /// `InputConnection.getTextAfterCursor(n)` — n UTF-16 units of
    /// text AFTER the cursor that is NOT part of the active
    /// composition. Same rationale as [textBeforeCursor].
    fun textAfterCursor(n: Int): CharSequence {
        val boundary = if (composingEnd > selectionEnd) composingEnd else selectionEnd
        val start = relInWindow(boundary) ?: return ""
        val end = (start + n).coerceAtMost(text.length)
        return text.substring(start, end)
    }

    /// `InputConnection.getSelectedText()` — text between selection
    /// start/end. Empty when there's no selection (cursor only).
    fun selectedText(): CharSequence {
        if (selectionEnd <= selectionStart) return ""
        val start = relInWindow(selectionStart) ?: return ""
        val end = relInWindow(selectionEnd) ?: return ""
        return text.substring(start, end)
    }

    /// `InputConnection.getExtractedText()` — full snapshot the IME
    /// uses for fullscreen extract mode AND for sanity checks.
    /// Returning the mirrored window is enough; IMEs handle short
    /// snapshots gracefully (Gboard, Swiftkey, Samsung all do).
    fun extractedText(): ExtractedText {
        val out = ExtractedText()
        out.text = SpannableStringBuilder(text)
        out.startOffset = windowStart
        // Selection start/end in ExtractedText are RELATIVE to startOffset.
        out.selectionStart = (selectionStart - windowStart).coerceIn(0, text.length)
        out.selectionEnd = (selectionEnd - windowStart).coerceIn(0, text.length)
        out.partialStartOffset = -1
        out.partialEndOffset = -1
        out.flags = 0
        return out
    }

    companion object {
        const val TAG = "zdroid_ime"

        /// Stand-in when Rust hasn't pushed any state yet (e.g., the
        /// editor isn't focused). Lets the IME's queries return empty
        /// strings rather than throwing.
        val EMPTY: ImeTextState =
            ImeTextState(text = "", windowStart = 0, selectionStart = 0, selectionEnd = 0, composingStart = -1, composingEnd = -1)
    }
}
