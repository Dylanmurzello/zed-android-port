package com.zdroid

import android.app.Activity
import android.view.InputDevice
import android.view.MotionEvent
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

/// Whether Android recognized this as a gesture on a trackpad, such as a two-finger
/// scroll or a pinch, and reports it with fake fingers. The classifications of a
/// touchscreen's events have that source instead.
internal val MotionEvent.isTrackpadGesture: Boolean
    get() = isFromSource(InputDevice.SOURCE_MOUSE) &&
        classification != MotionEvent.CLASSIFICATION_NONE

/// Sends this event to Rust over JNI, for a window without GameActivity's input
/// queue, or one whose classification that queue would lose.
internal fun MotionEvent.forwardTo(windowId: Long) {
    if (pointerCount <= 0) return
    val xs = FloatArray(pointerCount)
    val ys = FloatArray(pointerCount)
    val ids = IntArray(pointerCount)
    val toolTypes = IntArray(pointerCount)
    for (i in 0 until pointerCount) {
        xs[i] = getX(i)
        ys[i] = getY(i)
        ids[i] = getPointerId(i)
        toolTypes[i] = getToolType(i)
    }
    NativeBridge.nativeOnMotionEvent(
        windowId,
        actionMasked,
        actionIndex,
        source,
        buttonState,
        metaState,
        flags,
        classification,
        // ACTION_SCROLL (the mouse wheel) carries its delta on these axes. They're
        // zero on other events, and Rust only reads them for a scroll.
        getAxisValue(MotionEvent.AXIS_VSCROLL),
        getAxisValue(MotionEvent.AXIS_HSCROLL),
        xs,
        ys,
        ids,
        toolTypes,
    )
}

/// Draws behind the system bars and the cutout, so gpui's surface gets the
/// whole window, and hides the bars until a swipe brings them back, as an editor
/// on a desktop does.
internal fun Activity.enterEdgeToEdge() {
    WindowCompat.setDecorFitsSystemWindows(window, false)
    WindowInsetsControllerCompat(window, window.decorView).apply {
        hide(WindowInsetsCompat.Type.systemBars())
        systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
    }
}
