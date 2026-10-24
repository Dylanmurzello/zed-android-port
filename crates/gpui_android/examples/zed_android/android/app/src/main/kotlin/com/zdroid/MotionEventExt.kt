package com.zdroid

import android.app.Activity
import android.os.Build
import android.view.InputDevice
import android.view.MotionEvent
import android.view.Surface

/// Sum all historical + current samples of a relative MotionEvent axis.
/// `AXIS_RELATIVE_X` / `AXIS_RELATIVE_Y` are NOT accumulated across
/// batched samples by the framework; `getAxisValue` returns only the
/// most recent. Without summing the historical samples fast finger
/// motion loses ~80% of its travel (Tab S9 Ultra trackpad batches
/// ~6-10 samples per event).
internal fun sumRelativeAxis(event: MotionEvent, axis: Int, pointerIndex: Int): Float {
    var sum = 0f
    val historySize = event.historySize
    for (h in 0 until historySize) {
        sum += event.getHistoricalAxisValue(axis, pointerIndex, h)
    }
    sum += event.getAxisValue(axis, pointerIndex)
    return sum
}

/// [sumRelativeAxis] for both axes, in screen orientation. Pointer
/// capture hands over the kernel's axes without applying the display
/// rotation. That suits a mouse, which the user holds in screen
/// orientation, but the Samsung Book Cover trackpad reports in the
/// panel's natural portrait frame, so in landscape every motion came
/// out turned 90 degrees (moving right sent the cursor down).
/// Finger-driven deltas therefore get the display rotation applied.
internal fun capturedRelativeDelta(
    event: MotionEvent,
    pointerIndex: Int,
    displayRotation: Int,
): Pair<Float, Float> {
    val dx = sumRelativeAxis(event, MotionEvent.AXIS_RELATIVE_X, pointerIndex)
    val dy = sumRelativeAxis(event, MotionEvent.AXIS_RELATIVE_Y, pointerIndex)
    // The Book Cover trackpad sometimes reports SOURCE_MOUSE, but its
    // tool type stays FINGER, unlike a real mouse.
    val fromTrackpad = event.source and InputDevice.SOURCE_TOUCHPAD != 0 ||
        event.getToolType(pointerIndex) == MotionEvent.TOOL_TYPE_FINGER
    if (!fromTrackpad) return dx to dy
    return when (displayRotation) {
        Surface.ROTATION_90 -> dy to -dx
        Surface.ROTATION_180 -> -dx to -dy
        Surface.ROTATION_270 -> -dy to dx
        else -> dx to dy
    }
}

internal fun Activity.displayRotation(): Int {
    val currentDisplay = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
        display
    } else {
        @Suppress("DEPRECATION")
        windowManager.defaultDisplay
    }
    return currentDisplay?.rotation ?: Surface.ROTATION_0
}

/// Mouse pointer-acceleration curve: base sensitivity multiplier plus a
/// gentle quadratic boost capped well below the touch trackpad's 4x.
/// Under pointer capture Android bypasses its own acceleration and hands
/// us raw device counts, so without this the cursor crawls on a high-res
/// panel. Shared by MainActivity (primary window) and ExtraWindowActivity
/// (settings / spawned windows) so the cursor feels identical in both.
///   |d|=1  -> ~1.6   (precision when slow)
///   |d|=10 -> ~22
///   |d|=30 -> ~96    (boost capped at 2x)
internal fun accelerateMouse(delta: Float): Float {
    val magnitude = kotlin.math.abs(delta)
    val direction = kotlin.math.sign(delta)
    val boost = kotlin.math.min(1f + magnitude * magnitude * MOUSE_ACCEL_COEF, MOUSE_ACCEL_CAP)
    return direction * magnitude * MOUSE_SENSITIVITY * boost
}

private const val MOUSE_SENSITIVITY = 1.6f
private const val MOUSE_ACCEL_COEF = 0.004f
private const val MOUSE_ACCEL_CAP = 2.0f
