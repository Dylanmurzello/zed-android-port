package com.zdroid

import android.content.Context
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

/// Host for a single secondary gpui window. Spawned by Rust via JNI
/// `startActivity(Intent(...))` from `multi_window::launch_extra_activity`,
/// with the gpui `WindowId` passed as the `com.zdroid.window_id`
/// long extra. On freeform-windowing devices (DeX, Pixel desktop windowing,
/// Android 16 Desktop Mode, ChromeOS) the OS provides native chrome —
/// close X, drag bar, resize handles. On phones each Activity lives in its
/// own Recents task instead.
///
/// Lifecycle:
/// - `onCreate` registers this Activity instance with Rust via
///   `NativeBridge.nativeOnExtraActivityCreated`. Rust stores a `GlobalRef`
///   keyed by the window id so it can later issue `finishAndRemoveTask` for
///   gpui-initiated close.
/// - `SurfaceHolder.Callback` fires through `NativeBridge.nativeOnExtraSurface*`
///   — same JNI bridge as the primary surface, just keyed by window id.
/// - `OnTouchListener` forwards `MotionEvent`s through
///   `NativeBridge.nativeOnExtraTouchEvent`.
/// - `onDestroy` notifies Rust via `nativeOnExtraActivityDestroyed`. If Rust
///   triggered the destruction (`finishAndRemoveTask` from the gpui side),
///   the registry entry is already gone and the notify is idempotent. If the
///   user clicked the OS chrome X, this is the path that drives gpui-side
///   `Window::remove_window()` via the registered `on_close` callback.
///
/// Native lib loading: handled by `ZedApplication.onCreate` before any
/// Activity instantiates. Do NOT add a per-Activity `companion object init`
/// block here.
///
/// Activity recreation: `configChanges` in the manifest is exhaustive
/// enough to keep this Activity alive across drag-resize, rotation, density
/// change, locale change, etc. — the system delivers `onConfigurationChanged`
/// instead of recreating. If a config we forgot to declare ever fires
/// recreation, `onDestroy` notifies Rust which tears down the gpui Window;
/// the user's window disappears, which is bad UX. Test by aggressive
/// drag-resize after any manifest change.
class ExtraWindowActivity : AppCompatActivity(), ImeHost {
    private var extraWindowId: Long = -1L
    /// Window id used by [ImeHostView] / [ZdroidInputConnection] to
    /// route IME events to the right gpui-side window. Mirrors
    /// [extraWindowId].
    override val imeWindowId: Long
        get() = extraWindowId
    private lateinit var surfaceView: SurfaceView

    /// Invisible 1x1 [ImeHostView] that owns the IME `InputConnection`
    /// for this Activity's gpui window. Installed in [onCreate]
    /// alongside the SurfaceView; Rust signals show / hide via JNI
    /// calls to [showIme] / [hideIme] which requestFocus on this view
    /// and invoke `InputMethodManager`.
    private var imeHostView: ImeHostView? = null

    // No `ExtraKeysView` for extra windows: the row is editor-
    // focused (arrows, Esc, Tab, Ctrl, Alt) and renders only
    // alongside the main pane. Settings / picker windows that
    // focus a text input get the soft keyboard but no row, since
    // the relevant inputs there (search field, etc.) don't need
    // code-navigation keys. Locked at no-op modifier state so
    // `ImeHost.extraKeysModifierState` always returns 0 here and
    // `ZdroidInputConnection.commitText` never intercepts.

    /// Mirror of the OS IME's visibility from our perspective. Same
    /// rationale as MainActivity: gpui's `set_input_handler` /
    /// `take_input_handler` fires per paint, so we filter repeats
    /// before touching `InputMethodManager`.
    private var imeShown: Boolean = false
    private var programmaticHidePending: Boolean = false
    private var programmaticShowPending: Boolean = false
    private var lastImeInsetBottom: Int = 0
    @Volatile
    private var imeManuallyDismissed: Boolean = false

    @Volatile
    private var imeTextState: ImeTextState? = null

    @Volatile
    override var currentImeMode: Int = ImeInputMode.CODE_EDITOR
        private set

    override fun getImeTextState(): ImeTextState? = imeTextState

    // No extras row in extra windows; `commitText` never needs to
    // intercept for a modifier here.
    override val extraKeysModifierState: Int = 0
    override fun clearExtrasPendingModifier() {}

    override var softKeyboardEnabled: Boolean = false
        private set

    @Suppress("unused")
    fun setSoftKeyboardEnabled(enabled: Boolean) {
        runOnUiThread {
            if (softKeyboardEnabled == enabled) return@runOnUiThread
            softKeyboardEnabled = enabled
        }
    }

    /// Cursor position in physical pixels relative to this Activity's
    /// SurfaceView. Mirrors MainActivity's state machine.
    private var cursorX: Float = 0f
    private var cursorY: Float = 0f

    /// Hardware-composited cursor sprite. Child SurfaceControl of
    /// `surfaceView` (API 29+). Null on older devices and prior to
    /// first pointer-capture acquire.
    private var cursorOverlay: CursorSurfaceControl? = null

    // Cursor modality is the process-wide [InputModality] flag.
    // See `MainActivity` for the longer note; settings/picker
    // windows inherit the app-wide modality so they don't need a
    // fresh pointer event after opening to show the cursor.
    private fun applyCursorVisibility() {
        cursorOverlay?.setVisible(InputModality.isPointer() && hasWindowFocus())
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Edge-to-edge to match MainActivity. On phone (non-freeform) this
        // makes the secondary surface fill the screen end-to-end. On
        // freeform-windowing devices the OS-managed chrome (close X, drag
        // bar) renders on its own decoration layer above this Activity, so
        // hiding system bars here doesn't strip the chrome — only the
        // status / nav strips that don't belong to the freeform window.
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat
                .BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }

        extraWindowId = intent.getLongExtra(EXTRA_WINDOW_ID, -1L)
        if (extraWindowId < 0L) {
            Log.e(TAG, "onCreate: missing or invalid $EXTRA_WINDOW_ID extra; finishing")
            finish()
            return
        }
        Log.i(TAG, "onCreate windowId=$extraWindowId")

        // Process-death recovery: if Android killed our process and brought
        // this Activity back from Recents, the gpui-side runtime has been
        // re-init'd from scratch and doesn't know about our windowId. Running
        // through the rest of onCreate would attach a SurfaceView that fires
        // JNI callbacks against a Rust runtime with no matching gpui Window —
        // touches do nothing, no rendering, ghost window. Detect early and
        // bail. The user can re-open via the main app.
        if (!NativeBridge.nativeIsExtraWindowKnown(extraWindowId)) {
            Log.w(TAG, "onCreate windowId=$extraWindowId not known to Rust runtime (resurrection?); finishing")
            finish()
            return
        }

        NativeBridge.nativeOnExtraActivityCreated(extraWindowId, this)

        val id = extraWindowId
        surfaceView = ScrollableSurfaceView(this).apply {
            // OPAQUE pixel format so Android's compositor treats this
            // SurfaceView's buffer as fully opaque regardless of alpha
            // bytes in the wgpu output. RGBA_8888 (the previous value)
            // flipped the compositor into alpha-aware mode, which let
            // gpui's anti-aliased text edges + transparent shadow/scrim
            // regions bleed the activity's windowBackground (or any
            // default light backing) through as a visible whiteish tint
            // across the settings/secondary-window area. MainActivity
            // (GameActivity) defaults to an opaque format and never had
            // the tint; only ExtraWindowActivity needed this explicit
            // override.
            holder.setFormat(android.graphics.PixelFormat.OPAQUE)
            holder.addCallback(SurfaceCallback(id))
            // Forward touches to native via JNI. Returning true claims the
            // gesture so it doesn't bubble up to the OS chrome (which would
            // try to re-route to the drag handle).
            setOnTouchListener { _, event ->
                forwardTouchEvent(id, event)
                true
            }
            // ACTION_HOVER_ENTER / ACTION_HOVER_MOVE / ACTION_HOVER_EXIT
            // (mouse moving over the surface without a button pressed) come
            // through OnHoverListener — NOT OnTouchListener. Without this,
            // gpui never sees a `MouseMove { pressed_button: None }`, which
            // is what scrollbar-on-hover, link previews, and any
            // hover-only UI affordance is gated on.
            setOnHoverListener { _, event ->
                forwardTouchEvent(id, event)
                true
            }
            // ACTION_SCROLL (mouse wheel + trackpad two-finger scroll) and
            // mouse button events on SOURCE_MOUSE come through
            // OnGenericMotionListener. Without this, scrolling with a
            // hardware mouse / trackpad over a settings/secondary window
            // is a no-op.
            //
            // CRITICAL: when pointer capture is active for a connected
            // trackpad / mouse, the captured-pointer pipeline (the
            // Activity-level `onGenericMotionEvent` override below)
            // owns those events. If this listener returns `true` for
            // captured events it claims them and the Activity handler
            // never fires — the cursor sprite never updates and the
            // editor gets raw absolute-coordinate hovers instead of
            // synthesized MouseMove events from the gesture state
            // machine. Return `false` for captured sources so the
            // event falls through to the Activity handler.
            setOnGenericMotionListener { _, event ->
                val source = event.source
                val isCaptureSource =
                    source and InputDevice.SOURCE_TOUCHPAD != 0
                        || source and InputDevice.SOURCE_MOUSE_RELATIVE != 0
                        || source and InputDevice.SOURCE_MOUSE != 0
                if (isCaptureSource && window.decorView.hasPointerCapture()) {
                    false
                } else {
                    forwardTouchEvent(id, event)
                    true
                }
            }
            // SurfaceView wants to be the focusable target for IME / key
            // events; AppCompatActivity's default content view doesn't
            // grant focus on its own.
            isFocusable = true
            isFocusableInTouchMode = true
            // Captured-pointer events arrive via the Activity-level
            // `onGenericMotionEvent` override below — same pattern as
            // MainActivity. We don't install an
            // `OnCapturedPointerListener` here because some Samsung
            // builds bypass that listener path when DeX windowing
            // is active.
        }
        setContentView(surfaceView)

        // IME host. Invisible 1x1 view that owns the InputConnection
        // for this extra window's gpui surface. Without it, focusing
        // a text input in the settings window or any other spawned
        // window leaves the gpui-side `set_input_handler` registered
        // but the OS soft keyboard never appears, because Android
        // dispatches IME events through the focused View — not the
        // SurfaceView, which doesn't override `onCreateInputConnection`.
        val imeHost = ImeHostView(this)
        addContentView(imeHost, android.view.ViewGroup.LayoutParams(1, 1))
        imeHostView = imeHost

        // Inset-transition listener — same rationale as MainActivity.
        // Tracks programmatic-vs-user IME dismissals so the
        // auto-show on text-input focus is correctly suppressed
        // once the user has manually closed the keyboard in this
        // window (Back press, swipe-down on the IME bar).
        androidx.core.view.ViewCompat.setOnApplyWindowInsetsListener(imeHost) { _, insets ->
            val imeBottom = insets.getInsets(
                androidx.core.view.WindowInsetsCompat.Type.ime()
            ).bottom
            val wasVisible = lastImeInsetBottom > 0
            val nowVisible = imeBottom > 0
            if (!wasVisible && nowVisible) {
                programmaticShowPending = false
                if (!imeShown) setImeShown(true)
                Log.i(TAG_IME, "WindowInsets[w=$extraWindowId]: IME shown (inset=$imeBottom)")
            } else if (wasVisible && !nowVisible) {
                if (programmaticHidePending) {
                    programmaticHidePending = false
                    Log.i(TAG_IME, "WindowInsets[w=$extraWindowId]: IME hidden (programmatic)")
                } else {
                    Log.i(
                        TAG_IME,
                        "WindowInsets[w=$extraWindowId]: IME hidden by user, marking manual-dismiss"
                    )
                    setImeManuallyDismissed(true)
                }
                setImeShown(false)
            }
            lastImeInsetBottom = imeBottom
            insets
        }
    }

    /// Update [imeShown] and push the value into Rust's
    /// `SOFT_KEYBOARD_VISIBLE` mirror so the pane keyboard button's
    /// `toggle_state` highlight reflects this window's IME state.
    /// The global atomic represents whichever Activity most recently
    /// transitioned — only one IME can be up across the app at a time,
    /// so a single global is still correct semantics.
    private fun setImeShown(shown: Boolean) {
        if (imeShown != shown) {
            imeShown = shown
            NativeBridge.nativeSetSoftKeyboardVisible(shown)
        }
    }

    /// JNI hook from
    /// `gpui_android::platform::tick_extras_row_enabled`. Kept as a
    /// no-op for extra windows so the Rust-side broadcast that
    /// fans out to every registered Activity has a valid receiver
    /// here. Extras rows render only on the primary editor pane.
    @Suppress("unused", "UNUSED_PARAMETER")
    fun setProgrammingExtrasRowEnabled(enabled: Boolean) {}

    private fun setImeManuallyDismissed(dismissed: Boolean) {
        if (imeManuallyDismissed != dismissed) {
            Log.i(TAG_IME, "imeManuallyDismissed[w=$extraWindowId]: $imeManuallyDismissed -> $dismissed")
            imeManuallyDismissed = dismissed
        }
    }

    /// Bring up the soft keyboard for this window. Called from Rust
    /// via JNI when gpui's `set_input_handler` fires on a text-input
    /// focus inside this Activity's gpui window. Suppressed while the
    /// user has manually dismissed the IME in this window.
    @Suppress("unused")
    fun showIme() {
        runOnUiThread {
            val host = imeHostView ?: run {
                Log.w(TAG_IME, "showIme[w=$extraWindowId]: imeHostView is null, skipping")
                return@runOnUiThread
            }
            if (imeManuallyDismissed) {
                Log.i(TAG_IME, "showIme[w=$extraWindowId] suppressed (user dismissed)")
                return@runOnUiThread
            }
            Log.i(
                TAG_IME,
                "showIme[w=$extraWindowId] imeShown=$imeShown hostFocused=${host.isFocused}"
            )
            if (imeShown) return@runOnUiThread
            if (!host.isFocused) host.requestFocus()
            programmaticShowPending = true
            WindowInsetsControllerCompat(window, window.decorView)
                .show(WindowInsetsCompat.Type.ime())
            setImeShown(true)
        }
    }

    /// Dismiss the soft keyboard.
    @Suppress("unused")
    fun hideIme() {
        runOnUiThread {
            Log.i(TAG_IME, "hideIme[w=$extraWindowId] imeShown=$imeShown")
            if (!imeShown) return@runOnUiThread
            programmaticHidePending = true
            WindowInsetsControllerCompat(window, window.decorView)
                .hide(WindowInsetsCompat.Type.ime())
            setImeShown(false)
        }
    }

    /// Toggle the IME — pane keyboard button entry point.
    @Suppress("unused")
    fun toggleIme() {
        runOnUiThread {
            val host = imeHostView ?: return@runOnUiThread
            val imm = getSystemService(Context.INPUT_METHOD_SERVICE)
                as android.view.inputmethod.InputMethodManager
            if (imeShown) {
                Log.i(TAG_IME, "toggleIme[w=$extraWindowId]: hiding (manual dismiss)")
                programmaticHidePending = true
                WindowInsetsControllerCompat(window, window.decorView)
                    .hide(WindowInsetsCompat.Type.ime())
                setImeShown(false)
                setImeManuallyDismissed(true)
            } else {
                Log.i(TAG_IME, "toggleIme[w=$extraWindowId]: showing (clearing manual-dismiss)")
                if (!host.isFocused) host.requestFocus()
                imm.showSoftInput(host, 0)
                setImeShown(true)
                setImeManuallyDismissed(false)
            }
        }
    }

    /// Switch input modes for this window's IME and force a
    /// `restartInput` so the IME re-reads `EditorInfo` (terminal vs
    /// code-editor `EditorInfo` differs in autocorrect / multi-line
    /// flags). Called from Rust via JNI when the focused target's
    /// kind changes.
    @Suppress("unused")
    fun restartImeForTarget(modeId: Int) {
        runOnUiThread {
            if (modeId != ImeInputMode.TERMINAL && modeId != ImeInputMode.CODE_EDITOR) {
                Log.w(TAG_IME, "restartImeForTarget[w=$extraWindowId]: unknown modeId=$modeId")
                return@runOnUiThread
            }
            if (currentImeMode == modeId) return@runOnUiThread
            Log.i(
                TAG_IME,
                "restartImeForTarget[w=$extraWindowId]: $currentImeMode -> $modeId"
            )
            currentImeMode = modeId
            setImeManuallyDismissed(false)
            val host = imeHostView ?: return@runOnUiThread
            val imm = getSystemService(Context.INPUT_METHOD_SERVICE)
                as android.view.inputmethod.InputMethodManager
            imm.restartInput(host)
        }
    }

    /// Push Rust's view of this window's text + selection into Kotlin
    /// so the IME's read-side queries (getTextBeforeCursor / etc.)
    /// return real values, and fire `updateSelection` so Gboard sees
    /// touch-driven cursor moves.
    @Suppress("unused")
    fun updateImeTextState(
        text: String,
        windowStart: Int,
        selectionStart: Int,
        selectionEnd: Int,
        composingStart: Int,
        composingEnd: Int,
    ) {
        imeTextState = ImeTextState(
            text = text,
            windowStart = windowStart,
            selectionStart = selectionStart,
            selectionEnd = selectionEnd,
            composingStart = composingStart,
            composingEnd = composingEnd,
        )
        runOnUiThread {
            val host = imeHostView ?: return@runOnUiThread
            val imm = getSystemService(Context.INPUT_METHOD_SERVICE)
                as android.view.inputmethod.InputMethodManager
            imm.updateSelection(host, selectionStart, selectionEnd, composingStart, composingEnd)
        }
    }

    override fun onDestroy() {
        Log.i(TAG, "onDestroy windowId=$extraWindowId")
        cursorOverlay?.release()
        cursorOverlay = null
        if (extraWindowId >= 0L) {
            NativeBridge.nativeOnExtraActivityDestroyed(extraWindowId)
        }
        super.onDestroy()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        // Mirror MainActivity: request capture when this window gains
        // focus and a trackpad/mouse is connected, release when focus
        // is lost. Without this, spawned windows fall back to
        // Samsung's gesture filter which mangles trackpad gestures
        // into single-finger fake-mouse events.
        if (hasFocus) {
            if (hasIndirectPointer()) {
                Log.i(TAG, "requestPointerCapture() windowId=$extraWindowId")
                window.decorView.requestPointerCapture()
            }
        } else {
            window.decorView.releasePointerCapture()
        }
        // Maintain SurfaceControl lifecycle on focus change but let
        // visibility derive from the input modality. See
        // `MainActivity.onWindowFocusChanged` for the rationale.
        if (::surfaceView.isInitialized && trackpadModeActive && hasFocus) {
            ensureCursorOverlay()
            cursorOverlay?.move(cursorX, cursorY)
        }
        applyCursorVisibility()
    }

    private fun hasIndirectPointer(): Boolean {
        val ids = InputDevice.getDeviceIds()
        for (id in ids) {
            val dev = InputDevice.getDevice(id) ?: continue
            val sources = dev.sources
            if (sources and InputDevice.SOURCE_TOUCHPAD != 0) return true
            if (sources and InputDevice.SOURCE_MOUSE != 0) return true
            if (sources and InputDevice.SOURCE_MOUSE_RELATIVE != 0) return true
        }
        return false
    }

    override fun onPointerCaptureChanged(hasCapture: Boolean) {
        super.onPointerCaptureChanged(hasCapture)
        Log.i(TAG, "onPointerCaptureChanged windowId=$extraWindowId hasCapture=$hasCapture")
        if (hasCapture) {
            val (w, h) = visibleBounds()
            cursorX = w / 2f
            cursorY = h / 2f
            ensureCursorOverlay()
            cursorOverlay?.move(cursorX, cursorY)
        }
        // Visibility is driven by [InputModality.isPointer()], not by
        // capture transitions. See `MainActivity.onPointerCaptureChanged`.
        applyCursorVisibility()
    }

    /// True while the user is in touch-trackpad mode and clicked
    /// into this extra window. Pushed from Rust via JNI on every
    /// `TRACKPAD_MODE_ENABLED` flip (and on this Activity's
    /// creation if mode was already on).
    private var trackpadModeActive: Boolean = false

    /// Called from Rust via JNI. Shows / hides this window's
    /// SurfaceControl cursor overlay for trackpad mode, mirroring
    /// the same method on `MainActivity` so the user gets a
    /// visible cursor on whichever window they're interacting with.
    @Suppress("unused")
    fun setTrackpadModeActive(active: Boolean) {
        runOnUiThread {
            Log.i(TAG, "setTrackpadModeActive($active) windowId=$extraWindowId")
            trackpadModeActive = active
            // `surfaceView` is a `lateinit var` set during onCreate
            // (after JNI-creation completes). Rust pushes the
            // trackpad-mode state right after
            // `nativeOnExtraActivityCreated` fires — which can race
            // ahead of surfaceView init. Guard against that: store
            // the desired state and apply once the surface is
            // ready (via [applyDeferredTrackpadCursor], called from
            // surfaceCreated).
            if (!::surfaceView.isInitialized) {
                Log.i(TAG, "setTrackpadModeActive: surfaceView not yet init, deferred")
                return@runOnUiThread
            }
            if (active) {
                ensureCursorOverlay()
                if (cursorOverlay != null) {
                    val (w, h) = visibleBounds()
                    if (cursorX == 0f && cursorY == 0f) {
                        cursorX = w / 2f
                        cursorY = h / 2f
                    }
                    cursorOverlay?.move(cursorX, cursorY)
                    InputModality.setPointer()
                }
            } else {
                InputModality.setNonPointer()
            }
            applyCursorVisibility()
        }
    }

    /// Called from this Activity's `SurfaceHolder.Callback` once
    /// `surfaceView` is fully initialized. If trackpad mode was
    /// already active when we registered (typical path: user
    /// opens settings while in trackpad mode), the cursor sprite
    /// build was deferred — apply it now.
    private fun applyDeferredTrackpadCursor() {
        if (!trackpadModeActive) return
        if (!::surfaceView.isInitialized) return
        if (!hasWindowFocus()) return
        ensureCursorOverlay()
        if (cursorOverlay != null) {
            val (w, h) = visibleBounds()
            if (cursorX == 0f && cursorY == 0f) {
                cursorX = w / 2f
                cursorY = h / 2f
            }
            cursorOverlay?.move(cursorX, cursorY)
            InputModality.setPointer()
            applyCursorVisibility()
        }
    }

    // (focus-gated cursor visibility is folded into the existing
    // `onWindowFocusChanged` override above)

    /// Position the cursor sprite at (x, y) physical pixels —
    /// pushed by Rust's touch trackpad SM after each single-finger
    /// drag delta.
    @Suppress("unused")
    fun setTrackpadCursorPosition(x: Float, y: Float) {
        runOnUiThread {
            if (!::surfaceView.isInitialized) return@runOnUiThread
            val (w, h) = visibleBounds()
            cursorX = x.coerceIn(0f, w - 1f)
            cursorY = y.coerceIn(0f, h - 1f)
            cursorOverlay?.move(cursorX, cursorY)
        }
    }

    private fun ensureCursorOverlay() {
        if (cursorOverlay != null) return
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return
        val displaySize = (CURSOR_SIZE_DP * resources.displayMetrics.density)
            .toInt()
            .coerceAtLeast(16)
        cursorOverlay = CursorSurfaceControl(this, surfaceView, displaySize)
    }

    private fun visibleBounds(): Pair<Float, Float> {
        val w = surfaceView.width.toFloat().coerceAtLeast(1f)
        val h = surfaceView.height.toFloat().coerceAtLeast(1f)
        return w to h
    }

    /// Captured-mouse delivery path. Samsung/AOSP route captured
    /// relative-mouse motion, buttons, and wheel through
    /// processTrackballEvent -> onTrackballEvent, not onGenericMotionEvent.
    /// Without this override the physical mouse is dead in this window (the
    /// cursor never moves) even though the trackpad works via the touch
    /// path. Mirrors MainActivity.onTrackballEvent; the captured-pointer
    /// fix has to live in every window that hosts a gpui surface.
    override fun onTrackballEvent(event: MotionEvent): Boolean {
        if (window.decorView.hasPointerCapture()) {
            handleCapturedEvent(event)
            return true
        }
        return super.onTrackballEvent(event)
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        val source = event.source
        val isMouseRel = source and InputDevice.SOURCE_MOUSE_RELATIVE != 0
        val isTouchpad = source and InputDevice.SOURCE_TOUCHPAD != 0
        val isMouse = source and InputDevice.SOURCE_MOUSE != 0
        if ((isMouseRel || isTouchpad || isMouse)
            && window.decorView.hasPointerCapture()) {
            handleCapturedEvent(event)
            return true
        }
        return super.onGenericMotionEvent(event)
    }

    private fun handleCapturedEvent(event: MotionEvent) {
        if (!InputModality.isPointer()) {
            InputModality.setPointer()
            applyCursorVisibility()
        }
        if (event.actionMasked == MotionEvent.ACTION_MOVE) {
            val isHoldDragMultiTouch = event.pointerCount >= 2 &&
                NativeBridge.isHoldDragActive(extraWindowId)
            if (event.pointerCount == 1 || isHoldDragMultiTouch) {
                var sumRx = 0f
                var sumRy = 0f
                val limit = event.pointerCount
                val rotation = displayRotation()
                for (i in 0 until limit) {
                    val (dx, dy) = capturedRelativeDelta(event, i, rotation)
                    sumRx += dx
                    sumRy += dy
                }
                val (maxX, maxY) = visibleBounds()
                // Mouse-tuned pointer curve (shared accelerateMouse) so the
                // cursor in this window feels identical to the primary one.
                // Skipped while a button is held for precise click-drag.
                val dragging = event.buttonState != 0
                val moveX = if (dragging) sumRx else accelerateMouse(sumRx)
                val moveY = if (dragging) sumRy else accelerateMouse(sumRy)
                cursorX = (cursorX + moveX).coerceIn(0f, maxX - 1f)
                cursorY = (cursorY + moveY).coerceIn(0f, maxY - 1f)
                cursorOverlay?.move(cursorX, cursorY)
            }
        }
        forwardCapturedPointer(event)
    }

    /// JNI bridge: Rust's `cursor.rs::set_pointer_icon_inner` pushes the
    /// PointerIcon.TYPE_* id here so the SurfaceControl-based cursor
    /// sprite matches the gpui-requested style inside this window.
    @Suppress("unused")
    fun setCapturedCursorStyle(style: Int) {
        runOnUiThread {
            cursorOverlay?.setStyle(style)
        }
    }

    private fun forwardCapturedPointer(event: MotionEvent) {
        val n = event.pointerCount
        val xs = FloatArray(n)
        val ys = FloatArray(n)
        val rxs = FloatArray(n)
        val rys = FloatArray(n)
        val rotation = displayRotation()
        for (i in 0 until n) {
            xs[i] = event.getX(i)
            ys[i] = event.getY(i)
            val (dx, dy) = capturedRelativeDelta(event, i, rotation)
            rxs[i] = dx
            rys[i] = dy
        }
        val vs = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
        val hs = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
        NativeBridge.nativeOnExtraCapturedPointer(
            extraWindowId,
            event.actionMasked,
            event.source,
            event.buttonState,
            n,
            xs,
            ys,
            rxs,
            rys,
            vs,
            hs,
            cursorX,
            cursorY,
        )
    }

    /// Forward hardware key events to the gpui-side window. AppCompatActivity
    /// doesn't have GameActivity's native input queue, so without this
    /// override the OS routes KeyEvents to the focused View's default
    /// handler (which is a no-op for our SurfaceView) and gpui's
    /// `PlatformInput::KeyDown` never fires for editors in extra windows.
    /// Observed regression: Settings search bar focus worked (Editor's
    /// `Focused` event fired, `set_input_handler` registered), but typing
    /// did nothing because no KeyDown ever arrived.
    ///
    /// We forward every event up-front, then return `true` to claim it so
    /// Android's fallback IME routing doesn't try to steal keystrokes
    /// from an editor that thinks it has focus. ACTION_MULTIPLE events
    /// (synthesized soft-keyboard character sequences) are forwarded
    /// too; the Rust side drops them since gpui has no PlatformInput
    /// mapping for that action, which is the same policy as the primary
    /// window's translate_key_event uses.
    /// Hide the hardware-pointer cursor on direct-touch input,
    /// same as `MainActivity`. Re-shown by `handleCapturedEvent`
    /// on the next pointer activity. No-op while trackpad mode is
    /// active because touch IS the cursor input there.
    override fun dispatchTouchEvent(event: MotionEvent?): Boolean {
        if (event != null
            && !trackpadModeActive
            && InputModality.isPointer()
            && event.source and InputDevice.SOURCE_TOUCHSCREEN != 0
        ) {
            InputModality.setNonPointer()
            applyCursorVisibility()
        }
        return super.dispatchTouchEvent(event)
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_DOWN && InputModality.isPointer()) {
            InputModality.setNonPointer()
            applyCursorVisibility()
        }
        if (extraWindowId >= 0L) {
            NativeBridge.nativeOnExtraKeyEvent(
                extraWindowId,
                event.action,
                event.keyCode,
                event.metaState,
                event.repeatCount,
            )
            return true
        }
        return super.dispatchKeyEvent(event)
    }

    private inner class SurfaceCallback(private val id: Long) : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {
            Log.i(TAG, "surfaceCreated windowId=$id")
            NativeBridge.nativeOnExtraSurfaceCreated(id, holder.surface)
            // If trackpad mode was already active when this window
            // was created (user opened a settings/picker window
            // while already in trackpad mode), Rust pushed the
            // active state before `surfaceView` finished init and
            // we deferred the cursor build. Apply it now.
            applyDeferredTrackpadCursor()
        }

        override fun surfaceChanged(
            holder: SurfaceHolder,
            format: Int,
            width: Int,
            height: Int,
        ) {
            Log.i(TAG, "surfaceChanged windowId=$id ${width}x$height fmt=$format")
            NativeBridge.nativeOnExtraSurfaceChanged(id, holder.surface, format, width, height)
        }

        override fun surfaceDestroyed(holder: SurfaceHolder) {
            Log.i(TAG, "surfaceDestroyed windowId=$id")
            NativeBridge.nativeOnExtraSurfaceDestroyed(id)
        }
    }

    private fun forwardTouchEvent(id: Long, event: MotionEvent) {
        val pointerCount = event.pointerCount
        if (pointerCount <= 0) return
        val xs = FloatArray(pointerCount)
        val ys = FloatArray(pointerCount)
        val ids = IntArray(pointerCount)
        for (i in 0 until pointerCount) {
            xs[i] = event.getX(i)
            ys[i] = event.getY(i)
            ids[i] = event.getPointerId(i)
        }
        // ACTION_SCROLL (mouse wheel + trackpad two-finger scroll) carries
        // its delta on the AXIS_VSCROLL / AXIS_HSCROLL axes — getX/Y return
        // the pointer position, not the scroll amount. Read both axes
        // unconditionally; they're zero on non-scroll events and the Rust
        // translator only consumes them under the Scroll action arm.
        val vscroll = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
        val hscroll = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
        NativeBridge.nativeOnExtraTouchEvent(
            id,
            event.actionMasked,
            event.actionIndex,
            event.metaState,
            event.buttonState,
            event.eventTime,
            vscroll,
            hscroll,
            xs,
            ys,
            ids,
        )
    }

    companion object {
        private const val TAG = "zed_android_extra"
        private const val TAG_IME = "zdroid_ime"
        const val EXTRA_WINDOW_ID = "com.zdroid.window_id"
        /// Software cursor side length in dp — matches MainActivity.
        private const val CURSOR_SIZE_DP = 24
    }
}

/// `SurfaceView` subclass that advertises itself as scrollable to the
/// Android input subsystem. Android's input dispatcher decides whether
/// to synthesize an `ACTION_SCROLL` event from a trackpad two-finger
/// gesture by walking up from the pointer's hit-test target and asking
/// each `View` whether it `canScrollVertically()` / `canScrollHorizontally()`.
/// A bare `SurfaceView` returns `false` for both — the OS then falls
/// back to delivering the gesture as a single-pointer fake-mouse drag
/// (Down with `button_state=0` then Move ×N then Up), which gpui then
/// (correctly, given the input it sees) interprets as a click+drag.
///
/// Returning `true` for both axes flips Android's behavior: trackpad
/// two-finger swipes get translated to `ACTION_SCROLL` with VSCROLL /
/// HSCROLL axes set, which the existing primary + extra translators
/// already consume. Mouse wheel still delivers `ACTION_SCROLL` directly
/// (independent of this override).
///
/// Why this is on a custom subclass and not on the SurfaceView fields
/// directly: `View.canScrollVertically` is a `protected open fun` —
/// can only be overridden, not set, so we need a class.
private class ScrollableSurfaceView(context: Context) : SurfaceView(context) {
    override fun canScrollVertically(direction: Int): Boolean = true
    override fun canScrollHorizontally(direction: Int): Boolean = true
}
