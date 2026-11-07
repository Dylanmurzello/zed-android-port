package com.zdroid

/// What an [ImeHostView] and its [ZdroidInputConnection] ask their window.
/// [WindowIme] is the one implementation.
interface ImeHost {
    /// The gpui window IME events go to: 0 for the main one, else the id gpui
    /// gave an extra one.
    val imeWindowId: Long

    /// Current [ImeInputMode], which picks the `EditorInfo` flags.
    val currentImeMode: Int

    /// The text around the cursor, as Rust last pushed it, which the input
    /// connection's read methods answer from.
    fun getImeTextState(): ImeTextState?

    /// `KeyEvent.META_*` bits active on the [ExtraKeysView], pending or
    /// locked. A keyboard's `commitText` has no slot for modifiers, so a
    /// commit with one active is re-sent as a key event.
    val extraKeysModifierState: Int

    /// Clears the one-shot modifiers a key has used; locked ones stay.
    fun clearExtrasPendingModifier()

    /// Mirror of `android_input.on_screen_keyboard`.
    val softKeyboardEnabled: Boolean
}
