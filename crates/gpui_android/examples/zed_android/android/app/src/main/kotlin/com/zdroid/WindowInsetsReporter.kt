package com.zdroid

import android.util.Log
import android.view.View
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsAnimationCompat
import androidx.core.view.WindowInsetsCompat

/// Reports one window's insets to Rust through
/// [NativeBridge.nativeOnWindowInsetsChanged], which lays gpui out above the
/// part of the window the on-screen keyboard covers. Edge-to-edge windows
/// don't get `adjustResize`, so Rust does what it does for native views.
/// While the keyboard slides, this reports every frame of the animation, so
/// gpui's layout follows the keyboard's top edge.
///
/// [keyboardAccessoryHeight] is the height of a view the Activity floats on
/// the keyboard's top edge (`MainActivity`'s programming extras row), or 0
/// while none is shown; it counts as covered too. Call [refresh] when it
/// changes. [onImeBottomChanged] gets the keyboard's height whenever it
/// changes, every frame while it slides, to move such a view with it.
class WindowInsetsReporter(
    private val windowId: Long,
    private val keyboardAccessoryHeight: () -> Int = { 0 },
    private val onImeBottomChanged: (Int) -> Unit = {},
) {
    /// The keyboard's bottom inset, in physical pixels.
    var imeBottom: Int = 0
        private set

    /// How far the keyboard and the view on it reach up from the window's
    /// bottom edge, as last reported, in physical pixels.
    var obscuredBottom: Int = 0
        private set

    private var safeArea: Insets = Insets.NONE
    private var lastReported: IntArray? = null

    /// Set while a keyboard animation is prepared or running. Android
    /// dispatches the animation's end state to insets listeners before
    /// it starts, so the listener then keeps the last keyboard height and
    /// [WindowInsetsAnimationCompat.Callback.onProgress] reports the
    /// frames in between.
    private var imeAnimationPending = false
    private var animationFrames = 0

    /// Follows the keyboard's animations on [view], which must also pass
    /// its insets listener's insets to [onApplyWindowInsets].
    fun install(view: View) {
        val callback = object : WindowInsetsAnimationCompat.Callback(
            WindowInsetsAnimationCompat.Callback.DISPATCH_MODE_CONTINUE_ON_SUBTREE
        ) {
            override fun onPrepare(animation: WindowInsetsAnimationCompat) {
                if (animation.typeMask and IME != 0) imeAnimationPending = true
            }

            override fun onProgress(
                insets: WindowInsetsCompat,
                runningAnimations: MutableList<WindowInsetsAnimationCompat>,
            ): WindowInsetsCompat {
                if (runningAnimations.any { it.typeMask and IME != 0 }) {
                    animationFrames++
                    update(insets, insets.getInsets(IME).bottom)
                }
                return insets
            }

            override fun onEnd(animation: WindowInsetsAnimationCompat) {
                if (animation.typeMask and IME == 0) return
                imeAnimationPending = false
                val insets = ViewCompat.getRootWindowInsets(view) ?: return
                update(insets, insets.getInsets(IME).bottom)
                Log.i(
                    TAG,
                    "w=$windowId keyboard animation ended after $animationFrames frames: " +
                        describe(),
                )
                animationFrames = 0
            }
        }
        ViewCompat.setWindowInsetsAnimationCallback(view, callback)
    }

    /// Reports the insets an insets listener received.
    fun onApplyWindowInsets(insets: WindowInsetsCompat) {
        val ime = if (imeAnimationPending) imeBottom else insets.getInsets(IME).bottom
        if (update(insets, ime)) {
            Log.i(TAG, "w=$windowId insets: ${describe()}")
        }
    }

    /// Reports again after [keyboardAccessoryHeight] changed.
    fun refresh() {
        // Nothing to correct before the first insets arrive.
        if (lastReported == null) return
        if (report()) {
            Log.i(TAG, "w=$windowId keyboard accessory changed: ${describe()}")
        }
    }

    /// Returns whether the reported insets changed.
    private fun update(insets: WindowInsetsCompat, ime: Int): Boolean {
        safeArea = insets.getInsets(SAFE_AREA)
        if (ime != imeBottom) {
            imeBottom = ime
            onImeBottomChanged(ime)
        }
        return report()
    }

    private fun report(): Boolean {
        val obscured = imeBottom + keyboardAccessoryHeight()
        val values =
            intArrayOf(safeArea.left, safeArea.top, safeArea.right, safeArea.bottom, obscured)
        if (values.contentEquals(lastReported)) return false
        lastReported = values
        obscuredBottom = obscured
        NativeBridge.nativeOnWindowInsetsChanged(
            windowId,
            safeArea.left,
            safeArea.top,
            safeArea.right,
            safeArea.bottom,
            obscured,
        )
        return true
    }

    private fun describe(): String =
        "safe area (left, top, right, bottom)=(${safeArea.left}, ${safeArea.top}, " +
            "${safeArea.right}, ${safeArea.bottom}) ime bottom=$imeBottom " +
            "obscured bottom=$obscuredBottom"

    private companion object {
        const val TAG = "zdroid_insets"
        val IME = WindowInsetsCompat.Type.ime()
        val SAFE_AREA =
            WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
    }
}
