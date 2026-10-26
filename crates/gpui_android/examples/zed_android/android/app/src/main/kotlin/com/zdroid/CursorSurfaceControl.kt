package com.zdroid

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.PixelFormat
import android.graphics.PorterDuff
import android.os.Build
import android.util.Log
import android.view.Surface
import android.view.SurfaceControl
import android.view.SurfaceView
import androidx.annotation.RequiresApi

/// Hardware-composited cursor sprite. Lives as a child `SurfaceControl`
/// of the main SurfaceView's SurfaceControl, so SurfaceFlinger composes
/// the sprite over the main editor surface via HWC2 — same hardware
/// overlay path the system uses for the OS cursor on devices with a
/// dedicated cursor plane.
///
/// Motion: `move(x, y)` applies a `SurfaceControl.Transaction.setPosition`
/// and commits. SurfaceFlinger picks up the new position at the next
/// vsync; no gpui paint, no app redraw, no wgpu frame submission.
///
/// Style change: `setStyle(id)` re-locks the Surface's canvas and draws
/// the new sprite. Buffer size matches `displaySizePx`, so the
/// SurfaceControl layer is exactly cursor-sized — the entire raster is
/// `displaySizePx` × `displaySizePx`, scaled to that size from the
/// source bitmap at load time so the GPU sampler never gets involved.
///
/// API 29+ only. SurfaceView.getSurfaceControl(), SurfaceControl.Builder,
/// and SurfaceControl.Transaction were all added in Android 10. Older
/// devices fall back to no cursor sprite (the trackpad gestures still
/// work, the user just doesn't see a pointer). Gate at the call site —
/// don't instantiate this class below API Q.
@RequiresApi(Build.VERSION_CODES.Q)
internal class CursorSurfaceControl(
    context: Context,
    parentSurfaceView: SurfaceView,
    private val displaySizePx: Int,
) {
    private val arrowBitmap = loadCursor(context, R.drawable.cursor_arrow, displaySizePx)
    private val iBeamBitmap = loadCursor(context, R.drawable.cursor_ibeam, displaySizePx)
    private val handBitmap = loadCursor(context, R.drawable.cursor_hand, displaySizePx)
    private val grabBitmap = loadCursor(context, R.drawable.cursor_grab, displaySizePx)
    private val resizeHBitmap = loadCursor(context, R.drawable.cursor_resize_h, displaySizePx)
    private val resizeVBitmap = loadCursor(context, R.drawable.cursor_resize_v, displaySizePx)
    private val hotSpots: Map<Int, Pair<Int, Int>> = mapOf(
        STYLE_ARROW to (displaySizePx * 6 / 100 to displaySizePx * 6 / 100),
        STYLE_IBEAM to (displaySizePx / 2 to displaySizePx / 2),
        STYLE_VERTICAL_TEXT to (displaySizePx / 2 to displaySizePx / 2),
        STYLE_HAND to (displaySizePx * 35 / 100 to displaySizePx * 6 / 100),
        STYLE_GRAB to (displaySizePx / 2 to displaySizePx / 2),
        STYLE_GRABBING to (displaySizePx / 2 to displaySizePx / 2),
        STYLE_HORIZONTAL_DOUBLE_ARROW to (displaySizePx / 2 to displaySizePx / 2),
        STYLE_VERTICAL_DOUBLE_ARROW to (displaySizePx / 2 to displaySizePx / 2),
    )

    private val surfaceControl: SurfaceControl? = try {
        val parent = parentSurfaceView.surfaceControl
        if (!parent.isValid) {
            Log.w(TAG, "parent SurfaceControl not yet valid; cursor overlay will start hidden")
        }
        SurfaceControl.Builder()
            .setName("zdroid_cursor_overlay")
            .setParent(parent)
            .setBufferSize(displaySizePx, displaySizePx)
            .setFormat(PixelFormat.TRANSLUCENT)
            .setHidden(true)
            .build()
    } catch (t: Throwable) {
        Log.e(TAG, "SurfaceControl.Builder failed", t)
        null
    }

    private val drawSurface: Surface? = surfaceControl?.let { Surface(it) }

    /// Re-used per-call so we don't allocate a new Transaction (and its
    /// underlying native object) per motion event. SurfaceControl.Transaction
    /// methods return `this`, so chaining + apply() then re-using the same
    /// object is the documented pattern.
    private val transaction = SurfaceControl.Transaction()

    private var currentStyle: Int = STYLE_ARROW
    private var visible: Boolean = false
    /// Set to true the moment [release] runs. Every public mutation
    /// path (move, setStyle, setVisible, pending Arrow runnable) must
    /// short-circuit when this is true: the underlying SurfaceControl
    /// is gone, and calling SurfaceControl.Transaction methods against
    /// a released SC throws `IllegalStateException` from
    /// `checkNotReleased` and crashes the main thread. The race that
    /// motivates this: `setStyle(Arrow)` schedules an 80 ms debounced
    /// runnable; if pointer capture is lost within those 80 ms,
    /// MainActivity.onPointerCaptureChanged calls release(), and the
    /// runnable's deferred move() then explodes. The pendingArrow
    /// cancel below covers the common case, but a flag is required
    /// too: a fresh setStyle/move from gpui after release (e.g. a
    /// late JNI dispatch) would still race past the cancel.
    private var released: Boolean = false
    /// Last position passed to [move]. Cached so [setStyle] can
    /// re-apply position after a style swap: the new style's
    /// hot-spot offset differs from the old one, and without
    /// re-applying with the new offset the sprite stays at the
    /// raw `x - oldHotX` location and visually jumps by
    /// `newHotX - oldHotX` pixels on every hover transition.
    private var lastX: Float = 0f
    private var lastY: Float = 0f

    init {
        // Lift the cursor's z-order above the editor SurfaceView's own
        // surface so SurfaceFlinger composes it on top, and signal that
        // this layer wants the panel's max refresh rate so the cursor
        // tracks at 120Hz even when the editor surface is otherwise
        // idle. Without the frame-rate hint SurfaceFlinger's smart-
        // refresh drops the panel to 30Hz on idle and the cursor
        // (composed at the panel rate) inherits the drop.
        surfaceControl?.let { sc ->
            transaction
                .setLayer(sc, 1)
                .setFrameRate(sc, 120f, Surface.FRAME_RATE_COMPATIBILITY_DEFAULT)
                .apply()
        }
        // Paint the initial arrow sprite so the buffer is non-empty
        // before first show.
        paintCurrentStyle()
    }

    /// Snap the sprite to (x, y) in the parent SurfaceView's coordinate
    /// space (physical pixels). The hot-spot for the active style is
    /// subtracted so the sprite's tip points AT (x, y), not its
    /// top-left corner. Hot path: called per captured-pointer motion
    /// event (~200 Hz on Samsung trackpad).
    fun move(x: Float, y: Float) {
        if (released) return
        val sc = surfaceControl ?: return
        lastX = x
        lastY = y
        val (hotX, hotY) = hotSpots[currentStyle] ?: (0 to 0)
        transaction
            .setPosition(sc, x - hotX, y - hotY)
            .apply()
    }

    /// Pending Arrow application, scheduled via [arrowHandler]. Held
    /// so a non-Arrow style change can cancel an in-flight Arrow.
    private var pendingArrow: Runnable? = null
    private val arrowHandler = android.os.Handler(android.os.Looper.getMainLooper())

    fun setStyle(style: Int) {
        if (released) return
        // Cancel any pending Arrow transition first — whether this
        // style is Arrow or not, a fresh decision arrives now.
        pendingArrow?.let { arrowHandler.removeCallbacks(it) }
        pendingArrow = null

        if (currentStyle == style) return

        if (style == STYLE_ARROW) {
            // gpui's `reset_cursor_style` falls back to Arrow during
            // the click event itself (its `is_hovered_ignoring_last_input`
            // briefly returns false), then immediately restores the
            // real hover style on the next paint. Without filtering,
            // every click on a Hand-cursor button flashes Arrow-Hand
            // 1002→1000→1002 inside ~10ms. Defer Arrow by 80ms; if a
            // genuine non-Arrow style arrives during the delay we
            // cancel and apply that instead, swallowing the spurious
            // transient at no perceptible cost. A real Arrow (cursor
            // over empty space) lands 80ms later — still smooth.
            val runnable = Runnable {
                // Belt-and-suspenders: even if `release()` ran between
                // postDelayed and now, the [released] guard above the
                // move/paint calls keeps this from touching the dead
                // SurfaceControl. We still keep the removeCallbacks in
                // release() because that's the fast path (no allocation
                // and no main-thread dispatch).
                if (released) {
                    pendingArrow = null
                    return@Runnable
                }
                currentStyle = STYLE_ARROW
                paintCurrentStyle()
                move(lastX, lastY)
                pendingArrow = null
            }
            pendingArrow = runnable
            arrowHandler.postDelayed(runnable, 80L)
        } else {
            currentStyle = style
            paintCurrentStyle()
            // Re-apply position with the new style's hot-spot offset so
            // the cursor's pointing-tip stays anchored to (lastX, lastY)
            // across the style change. Without this, hovering between
            // an arrow region (hot-spot top-left) and an IBeam region
            // (hot-spot centered) makes the sprite jump by ~half its
            // size each transition.
            move(lastX, lastY)
        }
    }

    fun setVisible(visible: Boolean) {
        if (released) return
        if (this.visible == visible) return
        this.visible = visible
        val sc = surfaceControl ?: return
        transaction
            .setVisibility(sc, visible)
            .apply()
    }

    fun release() {
        if (released) return
        released = true
        // Yank any debounced Arrow runnable before tearing the
        // SurfaceControl down so the deferred move() can't fire
        // against a released SC. The [released] guards on the public
        // mutators above defend against any path the cancel doesn't
        // catch (a late JNI setStyle from gpui after release, etc.).
        pendingArrow?.let { arrowHandler.removeCallbacks(it) }
        pendingArrow = null
        try {
            drawSurface?.release()
        } catch (t: Throwable) {
            Log.w(TAG, "Surface.release threw", t)
        }
        surfaceControl?.let { sc ->
            try {
                transaction
                    .reparent(sc, null)
                    .apply()
                sc.release()
            } catch (t: Throwable) {
                Log.w(TAG, "SurfaceControl.release threw", t)
            }
        }
        try {
            transaction.close()
        } catch (t: Throwable) {
            Log.w(TAG, "Transaction.close threw", t)
        }
    }

    private fun paintCurrentStyle() {
        val surface = drawSurface ?: return
        val bitmap = bitmapForCurrentStyle()
        try {
            val canvas = surface.lockHardwareCanvas()
            canvas.drawColor(0, PorterDuff.Mode.CLEAR)
            canvas.drawBitmap(bitmap, 0f, 0f, null)
            surface.unlockCanvasAndPost(canvas)
        } catch (t: Throwable) {
            Log.e(TAG, "lockHardwareCanvas/draw/unlock failed", t)
        }
    }

    private fun bitmapForCurrentStyle(): Bitmap = when (currentStyle) {
        STYLE_IBEAM, STYLE_VERTICAL_TEXT -> iBeamBitmap
        STYLE_HAND -> handBitmap
        STYLE_GRAB, STYLE_GRABBING -> grabBitmap
        STYLE_HORIZONTAL_DOUBLE_ARROW -> resizeHBitmap
        STYLE_VERTICAL_DOUBLE_ARROW -> resizeVBitmap
        else -> arrowBitmap
    }

    companion object {
        private const val TAG = "zed_android_cursor"

        // PointerIcon.TYPE_* — IDs Rust passes from `cursor.rs` so the
        // captured cursor sprite matches what gpui asked for.
        const val STYLE_ARROW = 1000
        const val STYLE_IBEAM = 1008
        const val STYLE_VERTICAL_TEXT = 1009
        const val STYLE_HAND = 1002
        const val STYLE_HORIZONTAL_DOUBLE_ARROW = 1014
        const val STYLE_VERTICAL_DOUBLE_ARROW = 1015
        const val STYLE_GRAB = 1020
        const val STYLE_GRABBING = 1021

        private fun loadCursor(
            context: Context,
            @androidx.annotation.DrawableRes resId: Int,
            sizePx: Int,
        ): Bitmap {
            val raw = BitmapFactory.decodeResource(context.resources, resId)
            if (raw.width == sizePx && raw.height == sizePx) return raw
            val scaled = Bitmap.createScaledBitmap(raw, sizePx, sizePx, true)
            if (scaled !== raw) raw.recycle()
            return scaled
        }
    }
}
