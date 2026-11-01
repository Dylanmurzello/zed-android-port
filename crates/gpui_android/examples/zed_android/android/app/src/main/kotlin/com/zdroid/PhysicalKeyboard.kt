package com.zdroid

import android.content.Context
import android.hardware.input.InputManager
import android.net.Uri
import android.provider.Settings
import android.view.InputDevice

/// Whether a connected physical keyboard blocks the on-screen one. Process-
/// wide, refreshed by `MainActivity` (at startup, on input device changes
/// and when the Android setting changes), read by every Activity's
/// [ImeHostView] on each input connection and mirrored to Rust through
/// `NativeBridge.nativeSetPhysicalKeyboardBlocksIme`. Cached because
/// enumerating input devices is a system call per device, and
/// `onCreateInputConnection` runs on every focus change.
object PhysicalKeyboard {
    /// Android's "show on-screen keyboard while a physical keyboard is
    /// connected" setting: 1 when the user wants the on-screen keyboard
    /// anyway. The constant is hidden from the SDK.
    private const val SHOW_IME_WITH_HARD_KEYBOARD = "show_ime_with_hard_keyboard"

    val showImeWithHardKeyboardUri: Uri = Settings.Secure.getUriFor(SHOW_IME_WITH_HARD_KEYBOARD)

    @Volatile
    var blocksIme: Boolean = false
        private set

    /// Re-reads the connected devices and the setting; true when
    /// [blocksIme] changed.
    fun refresh(context: Context): Boolean {
        val blocks = isPhysicalKeyboardConnected(context) && !showImeWithHardKeyboard(context)
        if (blocks == blocksIme) return false
        blocksIme = blocks
        return true
    }

    private fun showImeWithHardKeyboard(context: Context): Boolean =
        Settings.Secure.getInt(context.contentResolver, SHOW_IME_WITH_HARD_KEYBOARD, 0) == 1

    /// Whether a non-virtual device with an alphabetic keyboard is
    /// connected that isn't also a mouse or touchpad. Seen on One UI: a
    /// Bluetooth mouse's keyboard interface and its pointer are reported as
    /// one device with an alphabetic keyboard, which counted the mouse alone
    /// as a keyboard, while Samsung's Book Cover keyboard reports its
    /// trackpad as a separate device, so it still counts. Known limitation:
    /// a keyboard whose built-in touchpad Android reports as the same device
    /// doesn't count.
    private fun isPhysicalKeyboardConnected(context: Context): Boolean {
        val inputManager = context.getSystemService(InputManager::class.java) ?: return false
        for (id in inputManager.inputDeviceIds) {
            val device = inputManager.getInputDevice(id) ?: continue
            if (device.isVirtual) continue
            if (device.keyboardType != InputDevice.KEYBOARD_TYPE_ALPHABETIC) continue
            if (device.supportsSource(InputDevice.SOURCE_MOUSE) ||
                device.supportsSource(InputDevice.SOURCE_MOUSE_RELATIVE) ||
                device.supportsSource(InputDevice.SOURCE_TOUCHPAD)
            ) {
                continue
            }
            return true
        }
        return false
    }
}
