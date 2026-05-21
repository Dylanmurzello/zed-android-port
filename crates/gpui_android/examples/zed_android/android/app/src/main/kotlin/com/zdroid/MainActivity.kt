package com.zdroid

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.drawable.Animatable
import android.net.ConnectivityManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.Process
import android.provider.DocumentsContract
import android.util.Log
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.animation.AccelerateDecelerateInterpolator
import android.widget.FrameLayout
import android.widget.ImageView
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.google.androidgamesdk.GameActivity
import java.io.File

/// SAF flows go through legacy `startActivityForResult` instead of
/// `ActivityResultLauncher` because `ActivityResultRegistry` silently
/// no-ops `launch()` when the host is in a non-STARTED lifecycle state,
/// which is the typical case when the call comes from a JNI thread driven
/// by gpui's render loop. AGDK's own SAF samples use the legacy path for
/// the same reason — `GameActivity` forwards `onActivityResult` correctly
/// to its Java host, and we get the result without any of the registry
/// gating.
///
/// Multi-window: this Activity hosts only the primary gpui window (the one
/// backing `android_app.native_window()` on the Rust side via GameActivity).
/// Every secondary `cx.open_window` is hosted by a separate
/// [ExtraWindowActivity] launched via Intent, giving each window OS-managed
/// freeform chrome on devices that support it. See `multi_window.rs` and
/// `ExtraWindowActivity.kt`.
class MainActivity : GameActivity(), ImeHost {
    /// MainActivity is always gpui's primary window — id 0.
    override val imeWindowId: Long = 0L
    /// Splash overlay shown from `super.onCreate` until the gpui-side
    /// flips `nativeIsZedReady` after first paint. Sits above the
    /// GameActivity `SurfaceView` so the animated Zdroid sigil is
    /// visible the entire time gpui boots, hiding the SurfaceView's
    /// default-black buffer + the wgpu init latency. Removed after
    /// the ready fade.
    private var splashOverlay: FrameLayout? = null
    private val splashHandler = Handler(Looper.getMainLooper())
    private var splashRemoved: Boolean = false

    /// Focusable invisible view that owns the IME `InputConnection`.
    /// Installed in `onCreate`. Rust signals show/hide via JNI calls
    /// to `showIme()` / `hideIme()` on this Activity; those methods
    /// requestFocus on the host and invoke `InputMethodManager`.
    private var imeHostView: ImeHostView? = null

    /// Programming extras row (Esc/Tab/Ctrl/Alt/arrows). Inflated
    /// lazily on first enable so we don't pay the layout cost when
    /// the user has the setting off. Visibility additionally gates
    /// on `imeShown` so the row only appears alongside the soft
    /// keyboard.
    private var extraKeysView: ExtraKeysView? = null
    // Kotlin-side cache of `android_input.programming_extras_row`.
    // Defaults to false so a fresh Activity sits in the "row hidden"
    // state until the Rust→Kotlin push lands. The push runs on
    // `runOnUiThread` which queues on the main looper; during boot
    // the main thread can be busy with language/theme init for
    // hundreds of ms, leaving a window where this field's value
    // determines what the IME-show codepath does. Default-true used
    // to lose that race and inflate the row even when the user had
    // the setting off. Default-false biases the race direction so the
    // visible state matches the user's setting whether the push has
    // landed yet or not.
    private var programmingExtrasRowEnabled: Boolean = false

    /// Mirror of `android_input.on_screen_keyboard`. Read by
    /// [ImeHostView] to gate Android's IME auto-show on focus.
    /// Pushed from Rust's `tick_soft_keyboard_setting` reconciler.
    override var softKeyboardEnabled: Boolean = false
        private set

    @Suppress("unused")
    fun setSoftKeyboardEnabled(enabled: Boolean) {
        runOnUiThread {
            if (softKeyboardEnabled == enabled) return@runOnUiThread
            softKeyboardEnabled = enabled
        }
    }

    /// Mirror of the [ExtraKeysView] modifier state machine. The
    /// view stores the source-of-truth state internally; this is
    /// a published copy read by [extraKeysModifierState] so
    /// [ZdroidInputConnection.commitText] can attach the
    /// modifier to a single-character soft-keyboard commit.
    @Volatile
    private var extraKeysPendingMeta: Int = 0
    @Volatile
    private var extraKeysLockedMeta: Int = 0

    /// Mirrors the IME's visibility from our perspective so repeated
    /// `showIme()` calls within a single visible-IME session don't
    /// re-trigger requestFocus / showSoftInput. gpui's paint logic
    /// fires `set_input_handler` every frame while the editor holds
    /// text focus (take then set per frame, see
    /// `crates/gpui/src/window.rs` paint flow), so without this
    /// flag the IME would receive show / focus events 60+ times per
    /// second and flicker visibly.
    private var imeShown: Boolean = false

    /// Set right before we call `imm.hideSoftInputFromWindow` from
    /// our own code (hideIme / toggleIme). The WindowInsets listener
    /// consults this to distinguish "we asked the IME to close"
    /// (don't mark manual-dismiss) from "user closed it via Back /
    /// swipe" (mark manual-dismiss so auto-show stays suppressed).
    private var programmaticHidePending: Boolean = false

    /// Set right before we initiate a programmatic show. Helps the
    /// WindowInsetsListener distinguish the steady-state-0 inset
    /// (no IME) from a steady-state-0 inset WHILE we're mid-show
    /// (animation hasn't started or just started). Without this the
    /// listener fires "user dismissed" between our show call and
    /// the first positive inset, resetting our state. Cleared once
    /// the inset goes positive (confirmed show).
    private var programmaticShowPending: Boolean = false

    /// Last ime-bottom inset value observed by the listener.
    /// Listener fires on every inset change; we only treat the
    /// transition `positive → 0` as a real hide event (which can
    /// distinguish user-dismiss vs programmatic). A steady-state 0
    /// reading before/during a show animation must not be confused
    /// with a hide.
    private var lastImeInsetBottom: Int = 0

    /// Setter that wraps the `imeShown` mutation and ALSO pushes
    /// the value into Rust's `SOFT_KEYBOARD_VISIBLE` mirror. Every
    /// site that flips `imeShown` should go through here so the
    /// pane keyboard button's `toggle_state` highlight stays
    /// synchronized with the OS-side IME visibility.
    private fun setImeShown(shown: Boolean) {
        if (imeShown != shown) {
            imeShown = shown
            NativeBridge.nativeSetSoftKeyboardVisible(shown)
            updateExtrasRowVisibility()
        }
    }

    /// Reconcile the `ExtraKeysView`'s presence in the content view
    /// against the two gates: the user setting
    /// (`programmingExtrasRowEnabled`) and the OS-side IME state
    /// (`imeShown`). Called whenever either input changes. Inflates
    /// the view lazily on first enable, then toggles visibility on
    /// subsequent changes, then removes the view when the setting
    /// is turned off entirely so we don't pay the layout cost.
    private fun updateExtrasRowVisibility() {
        val shouldShow = programmingExtrasRowEnabled && imeShown
        if (shouldShow) {
            if (extraKeysView == null) {
                val view = ExtraKeysView(this) { pending, locked ->
                    extraKeysPendingMeta = pending
                    extraKeysLockedMeta = locked
                }
                val params = android.widget.FrameLayout.LayoutParams(
                    android.view.ViewGroup.LayoutParams.MATCH_PARENT,
                    android.view.ViewGroup.LayoutParams.WRAP_CONTENT,
                    android.view.Gravity.BOTTOM,
                )
                addContentView(view, params)
                view.translationY = -lastImeInsetBottom.toFloat()
                extraKeysView = view
            }
            extraKeysView?.visibility = View.VISIBLE
        } else {
            extraKeysView?.visibility = View.GONE
        }
    }

    /// JNI hook: Rust pushes the user's
    /// `android_input.programming_extras_row` setting here on every
    /// transition. We persist the new value and reconcile against
    /// `imeShown`. Called from
    /// `gpui_android::platform::tick_extras_row_enabled` via JNI.
    @Suppress("unused")
    fun setProgrammingExtrasRowEnabled(enabled: Boolean) {
        runOnUiThread {
            if (programmingExtrasRowEnabled == enabled) return@runOnUiThread
            programmingExtrasRowEnabled = enabled
            if (!enabled) {
                // Tear down completely so the disabled state is also
                // free of layout overhead, not just visually hidden.
                extraKeysView?.let { (it.parent as? android.view.ViewGroup)?.removeView(it) }
                extraKeysView = null
            }
            updateExtrasRowVisibility()
        }
    }

    /// Bring up the soft keyboard. Called from Rust via JNI on the
    /// edge transition into text-input focus. The first call within
    /// a focus session does the real work; repeats are filtered.
    /// Suppressed entirely when the user has manually dismissed the
    /// IME (see [toggleIme] + the WindowInsets listener installed
    /// in [onCreate]) — the user can re-summon via the pane keyboard
    /// toggle button or by tapping into a different text target
    /// (which triggers `restartImeForTarget` and clears the flag).
    @Suppress("unused")
    fun showIme() {
        runOnUiThread {
            val host = imeHostView ?: run {
                Log.w("zdroid_ime", "showIme: imeHostView is null, skipping")
                return@runOnUiThread
            }
            if (imeManuallyDismissed) {
                Log.i("zdroid_ime", "showIme suppressed (user dismissed)")
                return@runOnUiThread
            }
            Log.i(
                "zdroid_ime",
                "showIme called imeShown=$imeShown hostFocused=${host.isFocused}"
            )
            if (imeShown) return@runOnUiThread
            if (!host.isFocused) host.requestFocus()
            // Use WindowInsetsControllerCompat for the show path too
            // (matches hide). `imm.showSoftInput` requires focused
            // text-input AND has a documented "first call silently
            // fails" race when window state is mid-transition — the
            // observed symptom where toggleIme:showing was followed
            // by WindowInsets immediately reporting IME hidden because
            // imm.show didn't actually take effect. InsetsController
            // routes through the OS-level inset animation directly.
            programmaticShowPending = true
            androidx.core.view.WindowInsetsControllerCompat(window, window.decorView)
                .show(androidx.core.view.WindowInsetsCompat.Type.ime())
            setImeShown(true)
        }
    }

    /// Dismiss the soft keyboard. Called from Rust on the edge
    /// transition out of text-input focus.
    @Suppress("unused")
    fun hideIme() {
        runOnUiThread {
            Log.i("zdroid_ime", "hideIme called imeShown=$imeShown")
            if (!imeShown) return@runOnUiThread
            programmaticHidePending = true
            // Use WindowInsetsControllerCompat over
            // `imm.hideSoftInputFromWindow` — the Android docs flag
            // the latter as racy when focus / window-token state is
            // mid-transition (the documented "first call silently
            // fails, second call works" symptom). InsetsController
            // bypasses that race by going through the OS-level inset
            // animation path directly. AndroidX's compat shim covers
            // API 21+ (native path on API 30+).
            androidx.core.view.WindowInsetsControllerCompat(window, window.decorView)
                .hide(androidx.core.view.WindowInsetsCompat.Type.ime())
            setImeShown(false)
        }
    }

    /// True when the user explicitly dismissed the IME (Back press,
    /// IME-bar swipe, etc.) and we should suppress the auto-show
    /// that fires on every text-input focus transition. Cleared by:
    /// - [toggleIme] when the user taps the pane keyboard button
    ///   to bring the IME back up,
    /// - any `restartImeForTarget` call (focus moved to a different
    ///   input target — fresh context, fresh auto-show budget).
    @Volatile
    private var imeManuallyDismissed: Boolean = false

    fun isImeManuallyDismissed(): Boolean = imeManuallyDismissed

    fun setImeManuallyDismissed(dismissed: Boolean) {
        if (imeManuallyDismissed != dismissed) {
            Log.i("zdroid_ime", "imeManuallyDismissed: $imeManuallyDismissed -> $dismissed")
            imeManuallyDismissed = dismissed
        }
    }

    /// Toggle the IME. If currently shown, hides it AND marks the
    /// IME as manually-dismissed so the auto-show on text-input
    /// focus is suppressed until the user re-toggles. If currently
    /// hidden, shows the IME and clears the manually-dismissed
    /// flag so subsequent focuses behave normally again.
    @Suppress("unused")
    fun toggleIme() {
        runOnUiThread {
            val host = imeHostView ?: return@runOnUiThread
            val imm = getSystemService(Context.INPUT_METHOD_SERVICE)
                as android.view.inputmethod.InputMethodManager
            if (imeShown) {
                Log.i("zdroid_ime", "toggleIme: hiding (manual dismiss)")
                programmaticHidePending = true
                // Modern hide path — see hideIme rationale. Sidesteps
                // the `hideSoftInputFromWindow` first-call race that
                // produced the two-taps-to-dismiss regression.
                androidx.core.view.WindowInsetsControllerCompat(window, window.decorView)
                    .hide(androidx.core.view.WindowInsetsCompat.Type.ime())
                setImeShown(false)
                setImeManuallyDismissed(true)
            } else {
                Log.i("zdroid_ime", "toggleIme: showing (clearing manual-dismiss)")
                if (!host.isFocused) host.requestFocus()
                imm.showSoftInput(host, 0)
                setImeShown(true)
                setImeManuallyDismissed(false)
            }
        }
    }

    /// IME state mirror — written by Rust after every commit /
    /// compose / delete via [updateImeTextState], read by
    /// [ZdroidInputConnection]'s `getTextBeforeCursor` /
    /// `getTextAfterCursor` / `getSelectedText` /
    /// `getExtractedText` overrides. `@Volatile` because Rust pushes
    /// from a JNI thread while the IME may query on the UI thread.
    @Volatile
    private var imeTextState: ImeTextState? = null

    /// Currently-focused input target kind. Drives the `EditorInfo`
    /// returned by [ImeHostView.onCreateInputConnection]:
    ///
    /// - `ImeInputMode.TERMINAL`: Termux-style raw key stream
    ///   (`TYPE_TEXT_VARIATION_VISIBLE_PASSWORD | NO_SUGGESTIONS`).
    ///   Disables composition + autocorrect; each keystroke commits
    ///   directly so the PTY sees normal hardware-keyboard semantics.
    /// - `ImeInputMode.CODE_EDITOR`: NO_SUGGESTIONS + IME_MULTI_LINE
    ///   without VISIBLE_PASSWORD — kills autocorrect for code
    ///   tokens but preserves composition for CJK input.
    ///
    /// `@Volatile`: Rust JNI thread writes via [restartImeForTarget]
    /// while the UI thread reads in `onCreateInputConnection`.
    @Volatile
    override var currentImeMode: Int = ImeInputMode.CODE_EDITOR
        private set

    override fun getImeTextState(): ImeTextState? = imeTextState

    override val extraKeysModifierState: Int
        get() = extraKeysPendingMeta or extraKeysLockedMeta

    override fun clearExtrasPendingModifier() {
        extraKeysView?.consumePendingModifier()
    }

    /// Switch input modes and force the IME to re-read EditorInfo.
    /// Called from Rust via JNI when the focused input target's kind
    /// changes (e.g. user tapped from an editor pane into a terminal
    /// pane).
    ///
    /// Effect: `InputMethodManager.restartInput(imeHostView)` causes
    /// the framework to invoke `imeHostView.onCreateInputConnection`
    /// again with a fresh `EditorInfo`, and the IME service receives
    /// `onFinishInput` + `onStartInput(restarting=true)` — dropping
    /// any in-flight composition state that was anchored to the
    /// outgoing target. This is the canonical pattern the Android
    /// developer guide endorses for "one host view, multiple logical
    /// editors" architectures.
    @Suppress("unused")
    fun restartImeForTarget(modeId: Int) {
        runOnUiThread {
            if (modeId != ImeInputMode.TERMINAL && modeId != ImeInputMode.CODE_EDITOR) {
                Log.w("zdroid_ime", "restartImeForTarget: unknown modeId=$modeId, ignoring")
                return@runOnUiThread
            }
            if (currentImeMode == modeId) {
                Log.i("zdroid_ime", "restartImeForTarget: already in mode=$modeId, skipping restart")
                return@runOnUiThread
            }
            Log.i(
                "zdroid_ime",
                "restartImeForTarget: switching mode ${currentImeMode} -> $modeId"
            )
            currentImeMode = modeId
            // Focus moved to a different input target — fresh
            // auto-show budget. Any prior manual dismiss applied
            // to the outgoing target, not this one.
            setImeManuallyDismissed(false)
            val host = imeHostView ?: return@runOnUiThread
            val imm = getSystemService(Context.INPUT_METHOD_SERVICE)
                as android.view.inputmethod.InputMethodManager
            imm.restartInput(host)
        }
    }

    /// Push the current editor text + selection across to Kotlin so
    /// the IME's queries return real values. Rust calls this after
    /// every text change. We also fire `InputMethodManager.updateSelection`
    /// so Gboard / Swiftkey / etc. know the cursor moved — without
    /// this, they re-confirm by sending the same composition twice
    /// (the duplicate-letter bug). All offsets are UTF-16 code-unit
    /// positions in the full document; `windowStart` is where `text`
    /// begins in document coordinates.
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
            Log.i(
                "zdroid_ime",
                "updateImeTextState: textLen=${text.length} winStart=$windowStart " +
                    "sel=$selectionStart..$selectionEnd comp=$composingStart..$composingEnd"
            )
        }
    }

    private val splashPoll: Runnable = object : Runnable {
        override fun run() {
            if (NativeBridge.nativeIsZedReady()) {
                onZedReady()
                return
            }
            // 32ms = ~30Hz polling. Splash boot waits 2–30s typically,
            // so per-frame polling is overkill; 30Hz keeps the
            // animation smooth and the wake budget low.
            splashHandler.postDelayed(this, 32L)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        installSplashOverlay()
        // Edge-to-edge: tell the OS we want to draw behind status / nav bars
        // and the cutout area, so gpui's surface gets the full display
        // bounds. Without this, GameActivity respects system insets and the
        // ANativeWindow we render into is shorter than the screen — visible
        // as letterboxing under the status bar / above the nav bar on
        // 1080x2340 phones (Mi 10) and notch-cropping on tablets.
        //
        // We also hide the system bars by default and set
        // BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE so a downward swipe
        // temporarily reveals the status bar (notifications) without
        // leaving the editor — same UX a native desktop editor gives on
        // Wayland/macOS.
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat
                .BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }

        // IME host. Invisible 1x1 view that owns the InputConnection so
        // gpui's text input flows through `ZdroidInputConnection`. Lives
        // alongside GameActivity's SurfaceView; touch dispatch is
        // unaffected (touch goes through the NDK input queue, focus is
        // independent). MainActivity calls `showIme()` / `hideIme()` to
        // bring up / dismiss the keyboard when gpui signals
        // `set_input_handler` / `take_input_handler`.
        val host = ImeHostView(this)
        addContentView(host, android.view.ViewGroup.LayoutParams(1, 1))
        imeHostView = host

        // Detect when the IME is dismissed by the user (Back press,
        // swipe-down on the keyboard) rather than programmatically by
        // us. Without this, our `imeShown` flag and the OS's actual
        // visibility drift, and the user-dismiss intent is lost the
        // next time text-input focus reasserts (auto-show pops the
        // keyboard back up — exactly the annoyance the user reported).
        //
        // Mechanism: the `ime()` inset goes from non-zero to zero
        // whenever the IME closes. We compare to `programmaticHidePending`
        // (a flag set right before our own hide calls) to tell apart
        // "we asked it to close" from "user closed it".
        androidx.core.view.ViewCompat.setOnApplyWindowInsetsListener(host) { _, insets ->
            val imeBottom = insets.getInsets(
                androidx.core.view.WindowInsetsCompat.Type.ime()
            ).bottom
            val wasVisible = lastImeInsetBottom > 0
            val nowVisible = imeBottom > 0

            if (!wasVisible && nowVisible) {
                // 0 → positive: IME just opened. Confirms a show.
                programmaticShowPending = false
                if (!imeShown) setImeShown(true)
                Log.i("zdroid_ime", "WindowInsets: IME shown (inset bottom=$imeBottom)")
            } else if (wasVisible && !nowVisible) {
                // positive → 0: IME just closed. Distinguish source.
                if (programmaticHidePending) {
                    programmaticHidePending = false
                    Log.i(
                        "zdroid_ime",
                        "WindowInsets: IME hidden (programmatic, keeping manual-dismiss flag)"
                    )
                } else {
                    Log.i(
                        "zdroid_ime",
                        "WindowInsets: IME hidden by user (Back / swipe), marking manual-dismiss"
                    )
                    setImeManuallyDismissed(true)
                }
                setImeShown(false)
            }
            // Steady-state (no transition, e.g. inset stays 0 during
            // a show that hasn't animated yet, or stays positive
            // during typing) — do nothing. The previous bug was firing
            // "user dismissed" on the steady-state-0 reading right
            // after our show call, before the animation had started.

            // Edge-to-edge (setDecorFitsSystemWindows=false) means the
            // OS does NOT auto-translate content above the IME — the
            // IME draws over the bottom of our surface. Any view with
            // gravity=BOTTOM (the ExtraKeysView) would sit at the
            // screen bottom and be hidden behind the keyboard. Apply
            // the IME bottom inset as a negative translationY on the
            // row so it floats just above the keyboard, following the
            // IME show/hide animation smoothly.
            extraKeysView?.translationY = -imeBottom.toFloat()

            lastImeInsetBottom = imeBottom
            insets
        }

        // Pointer-capture probe. When the decor view gains focus we ask
        // Android for raw pointer events. The captured listener
        // stringifies every event and forwards it to Rust for logging
        // only; no synthesis yet. This is here to verify whether Samsung
        // Book Cover Keyboard's trackpad gesture overlay (which
        // collapses two-finger scroll into single-pointer relative
        // motion in non-DeX tablet mode, never firing ACTION_SCROLL)
        // sits above or below the AOSP gesture-recognizer layer that
        // `requestPointerCapture` disables. If captured events show
        // multi-touch with `pointerCount > 1` and proper `AXIS_RELATIVE_*`
        // values, we know we can synthesize scroll on this hardware. If
        // they look identical to the non-captured path, Samsung is
        // intercepting deeper than the AOSP layer and we'd need a
        // different approach.
        //
        // Captured events route to the *focused* View, not decorView.
        // GameActivity sets focus on its SurfaceView, so we install the
        // listener on whatever SurfaceView we find in the hierarchy
        // (decorView's) on top of decorView as a fallback. Setting on
        // both is harmless; whichever the system dispatches to wins.
        // Captured pointer events route through `onGenericMotionEvent`
        // on the Activity (overridden below) — Moonlight's pattern.
        // Avoids the View-level captured-pointer listener path which
        // requires manipulating SurfaceView focus state and on Samsung
        // One UI triggers the accessibility tint + key dispatch
        // regression.
    }

    /// Cursor position tracked in physical pixels (decorView coordinate
    /// space). Accumulated from each captured-pointer event's
    /// `AXIS_RELATIVE_X`/`AXIS_RELATIVE_Y` deltas; the same value drives
    /// `cursorOverlay.move(...)` (visible sprite, hardware-composited
    /// via SurfaceControl) and is forwarded via JNI as the canonical
    /// cursor position for the gpui-side editor.
    private var cursorX: Float = 0f
    private var cursorY: Float = 0f

    /// Hardware-composited cursor sprite. Lives as a child SurfaceControl
    /// of the GameActivity SurfaceView (API 29+). Null on older devices
    /// and during the brief window between Activity create and
    /// pointer-capture acquire.
    private var cursorOverlay: CursorSurfaceControl? = null

    /// Desktop-classic auto-hide: cursor disappears on the first
    /// keystroke and reappears on any pointer motion. Tracked so we
    /// only toggle visibility on edges, not on every key.
    // Cursor modality is the process-wide [InputModality] flag now,
    // not a per-Activity field. Per-Activity tracking drifted across
    // window transitions: opening a new ExtraWindowActivity via the
    // trackpad would reset its local flag to "no pointer seen yet"
    // and leave the cursor hidden until the user moved the trackpad
    // a second time. The global flag means the freshly-opened
    // window inherits the app's current modality immediately.

    /// Apply [InputModality] + window focus to the cursor sprite.
    /// The only place that calls `cursorOverlay?.setVisible`. Each
    /// Activity owns its own SurfaceControl overlay; without the
    /// `hasWindowFocus()` gate, the unfocused Activity would also
    /// draw its cursor and the user would see two sprites at once
    /// (one in MainActivity behind the settings window, one in the
    /// settings window itself).
    private fun applyCursorVisibility() {
        cursorOverlay?.setVisible(InputModality.isPointer() && hasWindowFocus())
    }

    /// Called from Rust via JNI (`set_pointer_icon_inner` in
    /// `crates/gpui_android/src/cursor.rs`). Dispatches to the UI
    /// thread because the SurfaceControl transaction has to run on a
    /// looper thread. No-op when the overlay isn't live (capture not
    /// active or API < 29).
    @Suppress("unused")
    fun setCapturedCursorStyle(style: Int) {
        runOnUiThread {
            cursorOverlay?.setStyle(style)
        }
    }

    /// True while Rust's `TRACKPAD_MODE_ENABLED` atomic is set —
    /// touch-screen virtual trackpad mode is on. Drives the
    /// SurfaceControl cursor overlay's visibility independently
    /// from the hardware-pointer-capture path (the two are OR'd:
    /// either one keeps the sprite on).
    private var trackpadModeActive: Boolean = false

    /// Called from Rust via JNI when the user toggles trackpad
    /// mode. Shows / hides the SurfaceControl cursor overlay and
    /// builds it lazily if this is the first time it's needed
    /// (the existing build path is gated on
    /// `onPointerCaptureChanged`; trackpad mode runs without
    /// hardware capture so it needs its own bootstrap).
    @Suppress("unused")
    fun setTrackpadModeActive(active: Boolean) {
        runOnUiThread {
            Log.i(TAG_CAPTURE, "setTrackpadModeActive($active)")
            trackpadModeActive = active
            if (active) {
                ensureCursorOverlay()
                if (cursorOverlay != null) {
                    val (w, h) = visibleBounds()
                    if (cursorX == 0f && cursorY == 0f) {
                        cursorX = w / 2f
                        cursorY = h / 2f
                    }
                    cursorOverlay?.move(cursorX, cursorY)
                    // Activating trackpad mode is the user explicitly
                    // asking for the cursor, so mark pointer as the
                    // current modality and let the central applier
                    // show the sprite.
                    InputModality.setPointer()
                }
            } else {
                // Deactivating trackpad mode flips modality back to
                // touch (the touchscreen is no longer a cursor
                // input). If hardware capture is also off, the cursor
                // genuinely has no reason to be visible.
                InputModality.setNonPointer()
            }
            applyCursorVisibility()
        }
    }

    /// Position the cursor sprite at (x, y) physical pixels. Called
    /// by the Rust touch trackpad state machine after every
    /// single-finger drag delta. Clamps to the visible surface.
    @Suppress("unused")
    fun setTrackpadCursorPosition(x: Float, y: Float) {
        runOnUiThread {
            val (w, h) = visibleBounds()
            cursorX = x.coerceIn(0f, w - 1f)
            cursorY = y.coerceIn(0f, h - 1f)
            cursorOverlay?.move(cursorX, cursorY)
        }
    }

    override fun onPointerCaptureChanged(hasCapture: Boolean) {
        super.onPointerCaptureChanged(hasCapture)
        Log.i(TAG_CAPTURE, "onPointerCaptureChanged hasCapture=$hasCapture")
        Diagnostic.recordTransition("capture", hasCapture.toString())
        if (hasCapture) {
            val (w, h) = visibleBounds()
            cursorX = w / 2f
            cursorY = h / 2f
            // Release + rebuild the overlay on every capture-regain so
            // we anchor to the *current* SurfaceView's SurfaceControl.
            // The OS can tear down and recreate the SurfaceView's
            // surface when another activity steals focus (SAF picker,
            // settings dialogs, etc.), and any SurfaceControl we
            // previously attached as a child of the old surface gets
            // orphaned by SurfaceFlinger. Rebuilding on regain is
            // cheap (small bitmap upload + one SurfaceControl
            // transaction) and bulletproof.
            cursorOverlay?.release()
            cursorOverlay = null
            ensureCursorOverlay()
            cursorOverlay?.move(cursorX, cursorY)
        }
        // Visibility is NOT touched here. It's driven purely by
        // `InputModality.isPointer()` (set by handleCapturedEvent,
        // dispatchTouchEvent, dispatchKeyEvent, setTrackpadModeActive)
        // and applied by `applyCursorVisibility`. Focus cycles (IME
        // show/hide, dialog open/close) trigger this callback but
        // shouldn't change what the user sees — if they were in
        // touch mode the cursor stays hidden across the cycle. The
        // call below re-applies the existing modality state to the
        // freshly-built overlay so a rebuild ends up in the right
        // visual state without needing per-rebuild flag bookkeeping.
        applyCursorVisibility()
    }

    /// Build the SurfaceControl overlay on first capture-gain. API 29+
    /// gated; older Android leaves the field null and the trackpad
    /// continues to work without a visible cursor sprite.
    private fun ensureCursorOverlay() {
        if (cursorOverlay != null) return
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return
        val surfaceView = findSurfaceView(window.decorView) ?: return
        val displaySize = (CURSOR_SIZE_DP * resources.displayMetrics.density)
            .toInt()
            .coerceAtLeast(16)
        cursorOverlay = CursorSurfaceControl(this, surfaceView, displaySize)
    }

    private fun visibleBounds(): Pair<Float, Float> {
        val sv = findSurfaceView(window.decorView)
        val w = (sv?.width ?: window.decorView.width).toFloat().coerceAtLeast(1f)
        val h = (sv?.height ?: window.decorView.height).toFloat().coerceAtLeast(1f)
        return w to h
    }

    private fun findSurfaceView(view: View): SurfaceView? {
        if (view is SurfaceView) return view
        if (view is ViewGroup) {
            for (i in 0 until view.childCount) {
                val found = findSurfaceView(view.getChildAt(i))
                if (found != null) return found
            }
        }
        return null
    }

    /// Attach an animated splash overlay above the GameActivity
    /// SurfaceView. Stays visible until the gpui-Rust side flips
    /// `nativeIsZedReady` (first paint completed), at which point
    /// `onZedReady` fades the overlay out and removes it. The
    /// overlay covers the SurfaceView's default-black buffer + the
    /// wgpu boot latency, so the user sees a continuous animation
    /// from cold start through to the editor's first frame instead
    /// of icon → black → editor.
    ///
    /// Why a sibling View overlay rather than a separate
    /// SplashActivity:
    ///   - A separate activity must finish before MainActivity is
    ///     visible, but gpui (and SurfaceView) can only init while
    ///     MainActivity is visible, so the transition unavoidably
    ///     drops the animation mid-boot.
    ///   - The View-overlay path normally triggers SurfaceView's
    ///     compositor flip to alpha-aware mode (cursor white-tint
    ///     regression). Sidestepped here because the wgpu surface
    ///     is already configured with `transparent: true` +
    ///     `set_clear_color` to opaque brand indigo; the wgpu
    ///     output is always fully opaque once it draws anything,
    ///     so alpha-aware compositing has nothing transparent to
    ///     bleed through.
    private fun installSplashOverlay() {
        if (splashOverlay != null) return
        val container = FrameLayout(this).apply {
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            )
            setBackgroundResource(R.color.zdroid_bg)
        }
        val displaySizePx = (200 * resources.displayMetrics.density).toInt()
        val iconParams = FrameLayout.LayoutParams(displaySizePx, displaySizePx).apply {
            gravity = android.view.Gravity.CENTER
        }
        val iconView = ImageView(this).apply {
            layoutParams = iconParams
            setImageResource(R.drawable.splash_icon_animated)
            contentDescription = null
        }
        container.addView(iconView)
        val decor = window.decorView as? ViewGroup
        decor?.addView(container)
        splashOverlay = container
        (iconView.drawable as? Animatable)?.start()
        splashHandler.post(splashPoll)
    }

    private fun onZedReady() {
        if (splashRemoved) return
        splashRemoved = true
        splashHandler.removeCallbacks(splashPoll)
        val overlay = splashOverlay ?: return
        // Fade alpha + scale up ~10% (the "ripple dissipates" exit
        // that echoes the launcher icon's brand motif). 350ms feels
        // intentional without stalling the user's first input.
        overlay.animate()
            .alpha(0f)
            .scaleX(1.10f)
            .scaleY(1.10f)
            .setDuration(350L)
            .setInterpolator(AccelerateDecelerateInterpolator())
            .withEndAction {
                (overlay.parent as? ViewGroup)?.removeView(overlay)
                splashOverlay = null
            }
            .start()
    }

    // installCapturedPointerListenerOnAll removed: we no longer
    // install the View-level captured-pointer listener anywhere.
    // Activity.onGenericMotionEvent below is the single capture path.

    /// Activity-level catch for captured pointer events. Per Moonlight's
    /// pattern (the only Android remote-desktop client that's solved
    /// trackpad input on Samsung tablets): captured events also arrive
    /// here when the window has pointer capture, regardless of which
    /// View has focus. This avoids the `isFocusableInTouchMode=true`
    /// trap that triggers Samsung One UI's accessibility tint and
    /// breaks GameActivity's key dispatch.
    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        // Outer dispatch probe. If this fires but onGenericMotionEvent
        // doesn't, an intercept higher in the View hierarchy is
        // swallowing the event (Samsung One UI freeform window, third-
        // party accessibility service, etc.). Compare `M/disp.gen` vs
        // `M/act.gen` counts in the dump to triage.
        Diagnostic.recordMotion("disp.gen", event, window.decorView.hasPointerCapture(), false)
        return super.dispatchGenericMotionEvent(event)
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        val source = event.source
        val hasCapture = window.decorView.hasPointerCapture()
        val isMouseRel = source and InputDevice.SOURCE_MOUSE_RELATIVE != 0
        val isTouchpad = source and InputDevice.SOURCE_TOUCHPAD != 0
        val isMouse = source and InputDevice.SOURCE_MOUSE != 0
        val accepted = (isMouseRel || isTouchpad || isMouse) && hasCapture
        // Activity-level arrival probe. No source-class gate: the
        // earlier attempt gated on SOURCE_CLASS_POINTER (0x2) which
        // misses SOURCE_TOUCHPAD (CLASS_POSITION 0x8) and
        // SOURCE_MOUSE_RELATIVE (CLASS_TRACKBALL 0x4), so trackpad +
        // captured-mouse events never made it into the ring. Android
        // already routes non-pointer motion (touchscreen) to
        // dispatchTouchEvent rather than here, so leaving this
        // ungated does not flood the ring with finger motion.
        Diagnostic.recordMotion("act.gen", event, hasCapture, accepted)
        if (accepted) {
            handleCapturedEvent(event)
            return true
        }
        // Diagnostic for the "mouse pairs but doesn't move the cursor"
        // reports we keep getting from non-Samsung devices (Tab S11
        // MediaTek, OnePlus Pad ColorOS, etc.). Capture is held but
        // the source bits don't match our specific gate above —
        // probably the device reports a source-class combo we haven't
        // enumerated. Log the first occurrence per deviceId so a
        // reporter's dump exposes the exact flags and device name we
        // need to widen to. Gated on `SOURCE_CLASS_MASK` so the
        // detector covers pointer-class (mouse), position-class
        // (touchpad), trackball-class (captured mouse_relative), and
        // joystick-class — all the source classes that could plausibly
        // deliver an indirect-pointer event.
        if (source and InputDevice.SOURCE_CLASS_MASK != 0
            && window.decorView.hasPointerCapture()
            && rejectedPointerDevices.add(event.deviceId)
        ) {
            val device = InputDevice.getDevice(event.deviceId)
            val msg = "rejected pointer event during capture: deviceId=${event.deviceId} " +
                "name=\"${device?.name}\" source=0x${source.toString(16)} " +
                "action=${event.actionMasked} (extend " +
                "MainActivity.onGenericMotionEvent gate to accept this combo)"
            Log.w(TAG_CAPTURE, msg)
            // Also feed Diagnostic so the line is recoverable without
            // ADB: users tap Zdroid > Export Diagnostic and the share
            // sheet hands the dump to email/Drive.
            Diagnostic.record(TAG_CAPTURE, 'W', msg)
        }
        return super.onGenericMotionEvent(event)
    }

    /// `deviceId`s for which we've already emitted a "rejected
    /// pointer event during capture" log line. Prevents the
    /// diagnostic above from spamming logcat at the ~100Hz mouse-
    /// move rate on a reporter's broken-mouse device.
    private val rejectedPointerDevices = mutableSetOf<Int>()

    private fun handleCapturedEvent(event: MotionEvent) {
        // Any captured pointer activity makes pointer the current
        // modality; the central applier ensures the sprite is shown.
        if (!InputModality.isPointer()) {
            InputModality.setPointer()
            applyCursorVisibility()
        }
        if (event.actionMasked == MotionEvent.ACTION_MOVE) {
            // Cursor follows the moving finger in two cases:
            //   1. Single-finger motion (n=1): standard cursor drag.
            //   2. Multi-touch while hold-drag is active on the Rust
            //      side (queried via NativeBridge.isHoldDragActive):
            //      the user is selecting text by holding finger 1 and
            //      dragging finger 2, and expects the cursor to follow
            //      the second finger so they can see the selection
            //      growing.
            // For plain two-finger scroll (multi-touch but NOT in
            // hold-drag), cursor stays pinned per desktop standard.
            val isHoldDragMultiTouch = event.pointerCount >= 2 &&
                NativeBridge.isHoldDragActive(PRIMARY_WINDOW_ID)
            if (event.pointerCount == 1 || isHoldDragMultiTouch) {
                var sumRx = 0f
                var sumRy = 0f
                val limit = event.pointerCount
                for (i in 0 until limit) {
                    sumRx += sumRelativeAxis(event, MotionEvent.AXIS_RELATIVE_X, i)
                    sumRy += sumRelativeAxis(event, MotionEvent.AXIS_RELATIVE_Y, i)
                }
                val (maxX, maxY) = visibleBounds()
                cursorX = (cursorX + sumRx).coerceIn(0f, maxX - 1f)
                cursorY = (cursorY + sumRy).coerceIn(0f, maxY - 1f)
                cursorOverlay?.move(cursorX, cursorY)
            }
        }
        forwardCapturedPointer(event)
    }

    /// Hide the hardware-pointer cursor sprite when the user shifts
    /// to direct-touch input. Mirrors `dispatchKeyEvent`'s
    /// hide-on-keyboard logic so the cursor only sits on screen
    /// while the user is actually using a hardware pointer. NO-op
    /// when trackpad mode is active (touch IS the cursor input
    /// there) or when there's no cursor overlay to hide.
    /// Reappears on the next pointer activity via
    /// `handleCapturedEvent`.
    override fun dispatchTouchEvent(event: MotionEvent?): Boolean {
        // Direct-touch input flips modality to touch; skipped while
        // trackpad mode is on because in that mode touch IS the
        // cursor input (modality stays pointer).
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
        // Key input flips modality to keyboard; central applier
        // hides the sprite. ACTION_DOWN only so auto-repeat doesn't
        // re-apply on every UP/DOWN pair.
        if (event.action == KeyEvent.ACTION_DOWN && InputModality.isPointer()) {
            InputModality.setNonPointer()
            applyCursorVisibility()
        }
        return super.dispatchKeyEvent(event)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        Diagnostic.recordTransition("focus", hasFocus.toString())
        if (hasFocus) {
            if (hasIndirectPointer()) {
                Log.i(TAG_CAPTURE, "requestPointerCapture()")
                window.decorView.requestPointerCapture()
                Diagnostic.recordTransition("capture.request", "true")
            }
        } else {
            window.decorView.releasePointerCapture()
            Diagnostic.recordTransition("capture.release", "true")
        }
        // Move the sprite to the current cursor position so the
        // first visible-frame after a focus regain is correct, but
        // visibility itself is determined by `InputModality.isPointer()`
        // via the central applier. This prevents a focus cycle (IME
        // show/hide, settings window opening) from blindly re-showing
        // the cursor when the user was in touch mode.
        if (hasFocus && trackpadModeActive) {
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

    private fun forwardCapturedPointer(event: MotionEvent) {
        val n = event.pointerCount
        val xs = FloatArray(n)
        val ys = FloatArray(n)
        val rxs = FloatArray(n)
        val rys = FloatArray(n)
        for (i in 0 until n) {
            xs[i] = event.getX(i)
            ys[i] = event.getY(i)
            rxs[i] = sumRelativeAxis(event, MotionEvent.AXIS_RELATIVE_X, i)
            rys[i] = sumRelativeAxis(event, MotionEvent.AXIS_RELATIVE_Y, i)
        }
        val vs = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
        val hs = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
        // `cursorX` / `cursorY` are the canonical cursor position in
        // physical pixels (decorView coordinate space). Kotlin owns
        // this because it also has to position `cursorView` at the
        // same coords; passing it across JNI per event keeps the Rust
        // side from drifting against the visible sprite.
        NativeBridge.nativeOnCapturedPointer(
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

    @Suppress("unused")
    private fun describeCapturedPointer(event: MotionEvent): String {
        val sb = StringBuilder()
        sb.append("act=").append(MotionEvent.actionToString(event.actionMasked))
        sb.append(" src=0x").append(java.lang.Integer.toHexString(event.source))
        sb.append(" btn=0x").append(java.lang.Integer.toHexString(event.buttonState))
        sb.append(" n=").append(event.pointerCount)
        val pc = event.pointerCount.coerceAtMost(3)
        for (i in 0 until pc) {
            sb.append(" p").append(i).append("=(")
            sb.append("%.1f".format(event.getX(i))).append(",")
            sb.append("%.1f".format(event.getY(i))).append(" rx=")
            sb.append("%.2f".format(event.getAxisValue(MotionEvent.AXIS_RELATIVE_X, i))).append(" ry=")
            sb.append("%.2f".format(event.getAxisValue(MotionEvent.AXIS_RELATIVE_Y, i)))
            sb.append(" tt=").append(event.getToolType(i))
            sb.append(")")
        }
        val vs = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
        val hs = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
        if (vs != 0f || hs != 0f) {
            sb.append(" vscroll=").append("%.2f".format(vs))
            sb.append(" hscroll=").append("%.2f".format(hs))
        }
        return sb.toString()
    }

    @Suppress("unused") // called from Rust via JNI
    /**
     * Open an HTTPS URL in the user's default browser. Called via JNI from
     * the Rust side's `cx.open_url(...)`. Empty Android-platform stub before;
     * fix for the runtime picker's "Get module" button which delegates to
     * `cx.open_url(SPAWND_RELEASE_URL)`.
     *
     * Safe to call from any thread; `runOnUiThread` so `startActivity` runs
     * on main. Logs and swallows ActivityNotFoundException (rare on a stock
     * Android with a browser installed; never propagate back to gpui).
     */
    fun openUrl(url: String) {
        Log.i(TAG, "openUrl: $url")
        runOnUiThread {
            try {
                startActivity(
                    Intent(Intent.ACTION_VIEW, Uri.parse(url)).apply {
                        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    }
                )
            } catch (t: Throwable) {
                Log.e(TAG, "openUrl: startActivity ACTION_VIEW failed for $url", t)
            }
        }
    }

    fun launchOpenTree() {
        Log.i(TAG, "launchOpenTree() invoked")
        runOnUiThread {
            val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
                addFlags(
                    Intent.FLAG_GRANT_READ_URI_PERMISSION or
                        Intent.FLAG_GRANT_WRITE_URI_PERMISSION or
                        Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION
                )
                // Suggest the primary external storage root so the picker
                // lands somewhere familiar instead of "Recent".
                putExtra(
                    DocumentsContract.EXTRA_INITIAL_URI,
                    DocumentsContract.buildRootUri(
                        "com.android.externalstorage.documents",
                        "primary"
                    )
                )
            }
            try {
                startActivityForResult(intent, REQ_OPEN_TREE)
                Log.i(TAG, "startActivityForResult OPEN_DOCUMENT_TREE dispatched")
            } catch (t: Throwable) {
                Log.e(TAG, "OPEN_DOCUMENT_TREE dispatch threw", t)
                onPickerResult("")
            }
        }
    }

    @Suppress("unused") // called from Rust via JNI
    fun launchCreateDocument(suggestedName: String) {
        Log.i(TAG, "launchCreateDocument($suggestedName) invoked")
        runOnUiThread {
            val intent = Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "application/octet-stream"
                putExtra(Intent.EXTRA_TITLE, suggestedName)
                addFlags(
                    Intent.FLAG_GRANT_READ_URI_PERMISSION or
                        Intent.FLAG_GRANT_WRITE_URI_PERMISSION or
                        Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION
                )
            }
            try {
                startActivityForResult(intent, REQ_CREATE_DOCUMENT)
                Log.i(TAG, "startActivityForResult CREATE_DOCUMENT dispatched")
            } catch (t: Throwable) {
                Log.e(TAG, "CREATE_DOCUMENT dispatch threw", t)
                onPickerResult("")
            }
        }
    }

    /// Returns 1 if both READ + WRITE are already granted, 0 if a runtime
    /// dialog has been posted. Caller fires this once on boot and treats
    /// the call as best-effort: if the user denies, file-system reads of
    /// `/storage/emulated/0/...` will EACCES at the syscall layer with a
    /// clean error.
    @Suppress("unused") // called from Rust via JNI
    fun requestStoragePermissions(): Int {
        val needed = listOf(
            Manifest.permission.READ_EXTERNAL_STORAGE,
            Manifest.permission.WRITE_EXTERNAL_STORAGE,
        ).filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (needed.isEmpty()) {
            Log.i(TAG, "requestStoragePermissions: already granted")
            return 1
        }
        Log.i(TAG, "requestStoragePermissions: prompting for ${needed.joinToString(",")}")
        runOnUiThread {
            ActivityCompat.requestPermissions(this, needed.toTypedArray(), REQ_STORAGE_PERMS)
        }
        return 0
    }

    /// Returns Android's currently-active DNS server IPs as a comma-joined
    /// string. The Rust side writes them to /sdcard/.zed/r in resolv.conf
    /// format so Bun-compiled CLIs (whose c-ares is patched to read from
    /// /sdcard/.zed/r) can do DNS without proot. Falls back to empty
    /// string if no active network — caller layers in public-DNS defaults.
    @Suppress("unused") // called from Rust via JNI
    fun getActiveDnsServers(): String {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
            ?: return ""
        val network = cm.activeNetwork ?: return ""
        val props = cm.getLinkProperties(network) ?: return ""
        return props.dnsServers
            .mapNotNull { it.hostAddress }
            .joinToString(",")
    }

    /// Returns the running app's versionName (e.g. "0.2.0"). The in-app
    /// updater compares this against GitHub's `releases/latest` tag
    /// (e.g. "v0.2.1" with the `v` stripped) to decide whether to
    /// download an upgrade.
    @Suppress("unused") // called from Rust via JNI
    fun appVersionName(): String {
        return try {
            packageManager.getPackageInfo(packageName, 0).versionName ?: ""
        } catch (t: Throwable) {
            Log.w(TAG_UPDATE, "appVersionName: PackageManager threw", t)
            ""
        }
    }

    /// Hand a downloaded APK to Android's package installer. Rust
    /// calls this after the updater finishes writing the APK to
    /// `cacheDir/updater/zdroid-<tag>.apk`. We wrap the path in a
    /// FileProvider content:// URI (per the manifest provider
    /// declaration at `.updater.fileprovider`) so the installer can
    /// read across the app-private boundary; FLAG_GRANT_READ_URI_PERMISSION
    /// is what makes that grant explicit.
    ///
    /// Returns true on a successful intent dispatch (the installer UI
    /// will then take over and prompt the user). Returns false if the
    /// file is missing or the installer can't be started — Rust logs
    /// the failure but doesn't retry.
    @Suppress("unused") // called from Rust via JNI
    fun launchPackageInstaller(apkPath: String): Boolean {
        val file = File(apkPath)
        if (!file.exists()) {
            Log.e(TAG_UPDATE, "launchPackageInstaller: APK missing at $apkPath")
            return false
        }
        return try {
            val uri = FileProvider.getUriForFile(
                this,
                "com.zdroid.updater.fileprovider",
                file,
            )
            val intent = Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(uri, "application/vnd.android.package-archive")
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            startActivity(intent)
            true
        } catch (t: Throwable) {
            Log.e(TAG_UPDATE, "launchPackageInstaller dispatch failed", t)
            false
        }
    }

    /// Compose a diagnostic dump (device info, input devices, internal
    /// ring buffer) and hand it to Android's share sheet so the user
    /// can attach it to a bug report — no ADB, no PC required. Called
    /// from Rust via JNI when the Zdroid menu's "Export Diagnostic"
    /// entry fires. `extras` is an opaque text blob the Rust side
    /// supplies for the "## Runtime state" section (active runtime
    /// adapter id, PATH, etc.); empty string is fine if Rust has
    /// nothing to add.
    @Suppress("unused") // called from Rust via JNI
    fun exportDiagnostic(extras: String) {
        try {
            val file = Diagnostic.composeDump(this, extras.takeIf { it.isNotEmpty() })
            Diagnostic.save(this, file)
        } catch (t: Throwable) {
            Log.e(TAG_DIAG, "exportDiagnostic failed", t)
        }
    }

    /// Force a clean process exit when the Activity is destroyed.
    ///
    /// gpui_android has multiple static-state init paths (event channels,
    /// JNI globals, OnceLock guards) that assume process-scoped uniqueness.
    /// Android keeps the .so resident across Activity destroy/recreate
    /// cycles when memory pressure or AL_Kill reaps just the Activity but
    /// not the whole process. The next `android_main` re-entry then tries
    /// to re-initialize those statics, which either panics outright
    /// (multi_window event channel: "called twice") or silently leaves the
    /// new gpui state observing stale callbacks bound to the previous
    /// Activity.
    ///
    /// We've declared every config-change axis we care about in
    /// AndroidManifest.xml (`android:configChanges="orientation|...|
    /// uiMode|fontScale|..."`), so rotation, DeX, dark-mode flips, etc.
    /// don't destroy the Activity in the first place — those keep the
    /// process and Activity continuous, no re-entry. The only paths that
    /// reach `onDestroy` are genuine teardowns: user closed the app,
    /// system killed for memory, finishAndRemoveTask. For those, killing
    /// the process here guarantees the next launch starts fresh with
    /// zero stale static state.
    override fun onStart() {
        super.onStart()
        Diagnostic.recordTransition("lifecycle", "onStart")
    }

    override fun onResume() {
        super.onResume()
        Diagnostic.recordTransition("lifecycle", "onResume")
    }

    override fun onPause() {
        Diagnostic.recordTransition("lifecycle", "onPause")
        super.onPause()
    }

    override fun onStop() {
        Diagnostic.recordTransition("lifecycle", "onStop")
        super.onStop()
    }

    override fun onDestroy() {
        Log.i(TAG, "onDestroy isFinishing=$isFinishing — exiting process for clean restart")
        Diagnostic.recordTransition("lifecycle", "onDestroy")
        splashHandler.removeCallbacksAndMessages(null)
        cursorOverlay?.release()
        cursorOverlay = null
        super.onDestroy()
        Process.killProcess(Process.myPid())
    }


    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQ_STORAGE_PERMS) {
            return
        }
        val results = permissions.zip(grantResults.toTypedArray()).joinToString(",") { (perm, granted) ->
            "${perm.removePrefix("android.permission.")}=${if (granted == PackageManager.PERMISSION_GRANTED) "OK" else "DENIED"}"
        }
        Log.i(TAG, "onRequestPermissionsResult: $results")
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQ_OPEN_TREE && requestCode != REQ_CREATE_DOCUMENT) {
            return
        }
        if (resultCode != Activity.RESULT_OK) {
            Log.i(TAG, "picker cancelled (req=$requestCode resultCode=$resultCode)")
            onPickerResult("")
            return
        }
        val uri: Uri? = data?.data
        if (uri != null) {
            try {
                contentResolver.takePersistableUriPermission(
                    uri,
                    Intent.FLAG_GRANT_READ_URI_PERMISSION or
                        Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                )
            } catch (t: Throwable) {
                Log.w(TAG, "takePersistableUriPermission failed", t)
            }
        }
        onPickerResult(uri?.toString() ?: "")
    }

    private external fun onPickerResult(uriString: String)

    companion object {
        private const val TAG = "zed_android_saf"
        private const val TAG_CAPTURE = "zed_android_capture"
        private const val TAG_UPDATE = "zed_android_update"
        private const val TAG_DIAG = "zed_android_diag"
        private const val REQ_OPEN_TREE = 0xA1
        private const val REQ_CREATE_DOCUMENT = 0xA2
        private const val REQ_STORAGE_PERMS = 0xA3
        /// Software cursor side length in dp. Scaled by display
        /// density at instantiation time to give the sprite a
        /// consistent visual size across devices.
        private const val CURSOR_SIZE_DP = 24
        /// Matches `captured_pointer::PRIMARY_WINDOW_ID` on the Rust
        /// side. MainActivity always passes this when querying the
        /// per-window hold-drag flag; spawned `ExtraWindowActivity`
        /// instances pass their own `extraWindowId`.
        const val PRIMARY_WINDOW_ID: Long = 0
    }
}
