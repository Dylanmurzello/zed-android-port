//! IME / soft-keyboard bridge.
//!
//! Android IMEs (Gboard / Swiftkey / Samsung / etc.) talk to apps via
//! `InputConnection`. For typical `EditText`-hosted apps the framework
//! provides everything; for NDK / native-renderer apps (Zed) we host
//! our own. The Kotlin side (`ZdroidInputConnection.kt`,
//! `ImeHostView.kt`) subclasses `BaseInputConnection`; each IME
//! callback fires a JNI call into this module.
//!
//! Threading: JNI callbacks arrive on Android's UI thread.
//! `AndroidWindowState` (and its `PlatformInputHandler`) live in
//! `Rc<RefCell<_>>` on the game thread — not Send. So we marshal
//! every IME event onto an mpsc channel and the game thread drains it
//! each iteration of the main loop (same pattern as `multi_window`).
//! Show/hide IME goes the other direction —
//! Rust → Kotlin — via direct JNI calls on the Activity's `requestIme`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

/// Whether the on-screen keyboard is showing, which `WindowIme.kt` pushes
/// through `nativeSetSoftKeyboardVisible`, for the status bar keyboard button.
/// Process-wide, because only one keyboard can be up at a time.
pub(crate) static SOFT_KEYBOARD_VISIBLE: AtomicBool = AtomicBool::new(false);

pub(crate) fn soft_keyboard_visible() -> bool {
    SOFT_KEYBOARD_VISIBLE.load(Ordering::Acquire)
}

/// Whether a connected physical keyboard blocks the on-screen one, as
/// `MainActivity` last reported through `nativeSetPhysicalKeyboardBlocksIme`:
/// one is connected (`PhysicalKeyboard.kt` says what counts) and Android's
/// "show on-screen keyboard while a physical keyboard is connected" setting
/// is off. One UI's Samsung Keyboard doesn't show then, so the keyboard
/// isn't requested and the status bar hides its keyboard button.
pub(crate) static PHYSICAL_KEYBOARD_BLOCKS_IME: AtomicBool = AtomicBool::new(false);

pub(crate) fn physical_keyboard_blocks_ime() -> bool {
    PHYSICAL_KEYBOARD_BLOCKS_IME.load(Ordering::Acquire)
}

/// Mirrors the `android_input.on_screen_keyboard` user setting.
/// Written by a `cx.observe_global::<SettingsStore>` hook in
/// `zed_android::lib::main` — NOT from pane render, because the
/// onboarding flow runs without a Pane and would leave the atomic
/// stuck at its default while the user toggles the setting off in
/// Settings. Read by `reconcile_ime_visibility` to gate the
/// auto-show on text-input focus. Defaults to false so the early-
/// boot window (before the SettingsStore observer fires) doesn't
/// trip a stray auto-show; the observer overwrites with the user's
/// real setting within ms. Onboarding card + Android settings
/// page surface the toggle so opt-in is obvious.
pub(crate) static ON_SCREEN_KEYBOARD_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn on_screen_keyboard_enabled() -> bool {
    ON_SCREEN_KEYBOARD_ENABLED.load(Ordering::Acquire)
}

/// Mirrors `android_input.programming_extras_row`. Written by the
/// SettingsStore observer in `zed_android::lib::main`, pushed to
/// Kotlin via `Activity.setProgrammingExtrasRowEnabled` on each
/// settings change. Kotlin decides whether to inflate the
/// `ExtraKeysView` (Esc / Tab / Ctrl / Alt / arrow row). Defaults
/// to false to bias the boot-time race (Rust→Kotlin push runs on
/// `runOnUiThread` which queues on the main looper and can land
/// 100s of ms after first IME show) toward "row hidden" instead of
/// "row inflated then torn down".
pub(crate) static EXTRAS_ROW_ENABLED: AtomicBool = AtomicBool::new(false);

/// Mirrors `android_input.invert_scroll`. Written by the SettingsStore
/// observer in `zed_android::lib::main`. When set, the scroll deltas of a
/// trackpad's two-finger scroll and the mouse wheel are negated.
pub(crate) static INVERT_SCROLL: AtomicBool = AtomicBool::new(false);

pub(crate) fn invert_scroll_enabled() -> bool {
    INVERT_SCROLL.load(Ordering::Acquire)
}

/// `KeyCharacterMap.VIRTUAL_KEYBOARD`, the map for keys the on-screen keyboard
/// sends: they come from no physical keyboard with a layout of its own.
const VIRTUAL_KEYBOARD: i32 = -1;

/// IME window ids (0 for the main window) whose focused text field accepts
/// typed text, as gpui reports through `text_input_state_changed`. A focused
/// field that doesn't, such as an editor in vim's normal or visual mode,
/// gets soft-keyboard text as key events instead, so `j` moves down rather
/// than inserting "j". Kotlin's `commitText` asks through
/// `nativeImeRouteAsKeys` and converts the text with Android's
/// KeyCharacterMap.
static TEXT_INPUT_FOCUSED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

pub(crate) fn set_text_input_focused(ime_window_id: u64, focused: bool) {
    let mut windows = TEXT_INPUT_FOCUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    windows.retain(|window_id| *window_id != ime_window_id);
    if focused {
        windows.push(ime_window_id);
    }
}

fn route_as_keys(ime_window_id: u64) -> bool {
    !TEXT_INPUT_FOCUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&ime_window_id)
}

/// Negate a scroll delta when `invert_scroll` is set. No-op otherwise, so
/// default behavior is byte-identical for users who leave it off.
pub(crate) fn invert_scroll_delta(delta: gpui::ScrollDelta) -> gpui::ScrollDelta {
    if !invert_scroll_enabled() {
        return delta;
    }
    match delta {
        gpui::ScrollDelta::Pixels(p) => gpui::ScrollDelta::Pixels(gpui::point(-p.x, -p.y)),
        gpui::ScrollDelta::Lines(p) => gpui::ScrollDelta::Lines(gpui::point(-p.x, -p.y)),
    }
}

use android_activity::AndroidApp;
use anyhow::Context as _;
use futures::channel::mpsc;
use jni::JNIEnv;
use jni::objects::{JObject, JString};

use crate::window::AndroidWindowStatePtr;

/// Coarse classification of the focused input target. Drives the
/// `EditorInfo` returned by `ImeHostView.onCreateInputConnection` so
/// the IME (Gboard / Swiftkey / Samsung) configures itself for the
/// right input style:
///
/// - `Terminal`: each keystroke commits directly to the PTY; no
///   composition, no autocorrect, no prediction. Matches Termux's
///   `TYPE_TEXT_VARIATION_VISIBLE_PASSWORD | NO_SUGGESTIONS`
///   pattern (TerminalView.java line 280).
/// - `CodeEditor`: composition is useful (CJK input) but
///   suggestions / autocorrect are wrong for code. Use
///   `TYPE_CLASS_TEXT | NO_SUGGESTIONS | IME_MULTI_LINE`.
///   Crucially, do NOT use `VISIBLE_PASSWORD` here — that kills
///   CJK composition entirely.
///
/// Detected by probing `text_for_range`: terminals stub it to
/// `None`, editors return real content (even empty buffers return
/// `Some("")`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum ImeTargetKind {
    Terminal,
    CodeEditor,
}

impl ImeTargetKind {
    /// Marshalled int that crosses the JNI boundary into Kotlin's
    /// `ImeInputMode`. Keep in sync with `ImeInputMode.kt`.
    fn to_jni_int(self) -> i32 {
        match self {
            ImeTargetKind::Terminal => 0,
            ImeTargetKind::CodeEditor => 1,
        }
    }
}

/// Probe what kind of input target a handler represents. Caller
/// supplies the handler mutably (`text_for_range` takes &mut self).
/// Side-effect-free for both editor and terminal — just queries
/// existing state, doesn't write anything.
pub(crate) fn probe_target_kind(handler: &mut gpui::PlatformInputHandler) -> ImeTargetKind {
    let mut adjusted = None;
    match handler.text_for_range(0..1, &mut adjusted) {
        Some(_) => ImeTargetKind::CodeEditor,
        None => ImeTargetKind::Terminal,
    }
}

/// Tell Kotlin to switch IME modes and restart the input connection.
/// This causes `ImeHostView.onCreateInputConnection` to fire again
/// with a fresh `EditorInfo` derived from `kind`, and forces the IME
/// service to send `onFinishInput` + `onStartInput(restarting=true)`
/// — dropping any in-flight composition that was anchored to the
/// outgoing target. See research summary: Android docs explicitly
/// endorse this pattern for "single host view, multiple logical
/// editors" architectures (developer.android.com custom-text-editors
/// guide).
pub(crate) fn restart_input_for_kind(
    android_app: &AndroidApp,
    extra_window_id: Option<u64>,
    kind: ImeTargetKind,
) {
    log::info!(
        "ime::restart_input_for_kind w={extra_window_id:?} kind={:?}",
        kind
    );
    let result = crate::jni_env::call_window_method(
        android_app,
        extra_window_id,
        "restartImeForTarget",
        "(I)V",
        &[kind.to_jni_int().into()],
    );
    if let Err(err) = result {
        log::warn!("ime::restart_input_for_kind failed: {err:#}");
    }
}

/// Input events the IME wants the gpui side to apply. Each variant
/// corresponds to one `InputConnection` method and translates to one
/// `PlatformInputHandler` call (or hardware-key path for sendKeyEvent).
pub(crate) enum ImeEvent {
    /// `commitText(text, newCursorPosition)`. Final text the IME wants
    /// inserted at the cursor (or replacing any active composition).
    CommitText {
        text: String,
        new_cursor_position: i32,
    },
    /// `setComposingText(text, newCursorPosition)`. In-progress
    /// composition (CJK, gesture typing, prediction).
    SetComposingText {
        text: String,
        new_cursor_position: i32,
    },
    /// `finishComposingText()`. End of composition without further
    /// edits.
    FinishComposingText,
    /// `deleteSurroundingText(before, after)`. Backspace / delete span
    /// around the cursor.
    DeleteSurroundingText {
        before_length: i32,
        after_length: i32,
    },
    /// `sendKeyEvent(KeyEvent)`. Hardware-style key delivery from the
    /// IME (Enter, arrows, etc.). Routed through the existing
    /// `events::keyboard::translate_extra_key_event` translator.
    KeyEvent {
        action: i32,
        keycode: u32,
        meta_state: u32,
        repeat_count: i32,
    },
    /// `performEditorAction(actionId)` — Enter / Done / Next / Search.
    /// Not handled yet; logged for now.
    EditorAction { action_id: i32 },
}

/// Each IME event is paired with the `window_id` it was generated
/// for so the drainer can route to the right gpui window. `0` =
/// primary (`MainActivity`), nonzero = the registered
/// `ExtraWindowActivity` with that id. Without the pairing, an
/// IME event triggered from inside a settings window would land on
/// the editor in MainActivity instead.
static EVENT_TX: Mutex<Option<mpsc::UnboundedSender<(u64, ImeEvent)>>> = Mutex::new(None);

/// Construct a fresh sender/receiver pair. Returns the receiver for
/// the platform to drain. Safe to call multiple times (Activity-
/// recreation idempotent); each call drops the previous sender.
pub(crate) fn init_event_channel() -> mpsc::UnboundedReceiver<(u64, ImeEvent)> {
    let (tx, rx) = mpsc::unbounded();
    *EVENT_TX.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    rx
}

fn dispatch_event(window_id: u64, event: ImeEvent) {
    log::debug!("ime::dispatch_event w={window_id} {}", debug_event(&event));
    let guard = EVENT_TX.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(tx) = guard.as_ref() else {
        log::warn!("ime: event arrived before init_event_channel");
        return;
    };
    if let Err(err) = tx.unbounded_send((window_id, event)) {
        log::warn!("ime: dispatch_event failed: {err:#}");
    }
    crate::platform::wake_run_loop();
}

fn debug_event(event: &ImeEvent) -> String {
    match event {
        ImeEvent::CommitText {
            text,
            new_cursor_position,
        } => {
            format!("CommitText text={text:?} cursor={new_cursor_position}")
        }
        ImeEvent::SetComposingText {
            text,
            new_cursor_position,
        } => {
            format!("SetComposingText text={text:?} cursor={new_cursor_position}")
        }
        ImeEvent::FinishComposingText => "FinishComposingText".to_string(),
        ImeEvent::DeleteSurroundingText {
            before_length,
            after_length,
        } => {
            format!("DeleteSurroundingText before={before_length} after={after_length}")
        }
        ImeEvent::KeyEvent {
            action,
            keycode,
            meta_state,
            repeat_count,
        } => {
            format!(
                "KeyEvent action={action} keycode={keycode} meta={meta_state:#x} repeat={repeat_count}"
            )
        }
        ImeEvent::EditorAction { action_id } => format!("EditorAction id={action_id}"),
    }
}

/// Drain pending IME events, dispatching each one to its target
/// window's `PlatformInputHandler`. `primary_window` is the
/// MainActivity-backed gpui window (the receiver of events with
/// `window_id == 0`); `lookup_extra` resolves nonzero ids against
/// the platform's extra-window registry. Called once per platform
/// loop iteration on the game thread.
pub(crate) fn drain_ime_events(
    primary_window: Option<&AndroidWindowStatePtr>,
    lookup_extra: impl Fn(u64) -> Option<AndroidWindowStatePtr>,
    rx: &mut mpsc::UnboundedReceiver<(u64, ImeEvent)>,
) {
    while let Ok((window_id, event)) = rx.try_recv() {
        let target = if window_id == 0 {
            primary_window.cloned()
        } else {
            lookup_extra(window_id)
        };
        match target {
            Some(window_ptr) => apply_event(&window_ptr, event),
            None => {
                log::warn!(
                    "ime: dropping event for unknown window_id={window_id} ({})",
                    debug_event(&event),
                );
            }
        }
    }
}

/// Replace the currently-active composition span with `new_text` as
/// MARKED (composition) text. Mirrors the macOS `setMarkedText` flow:
/// the editor stores the text + a marked range, doesn't commit it,
/// and shows it with composition underline. A subsequent
/// `setComposingText` extends the SAME marked range (so `m` → `mo`
/// → `mor` replaces the prior composition, not appends). A final
/// `commitText` calls [`commit_composition`] which finalizes.
///
/// For terminals, this routes to `set_marked_text` — the text is
/// shown in the composition overlay but NOT sent to the PTY. Only
/// `commit_composition` sends to the PTY.
pub(crate) fn set_composition(window_ptr: &AndroidWindowStatePtr, new_text: &str) {
    let new_len = new_text.encode_utf16().count();
    let previous_start = window_ptr.state.borrow().ime_composition_start;
    let start = window_ptr.with_input_handler(|handler| {
        // `marked_text_range()` would be the authoritative source of where a
        // composition starts, but terminals don't implement it.
        let start = previous_start.unwrap_or_else(|| {
            handler
                .selected_text_range(false)
                .map_or(0, |selection| selection.range.start)
        });
        // `None` means "replace the entire active marked region" for the
        // editor, and "set the composition overlay" for the terminal. The
        // editor's `replace_and_mark_text_in_range` reads a `Some(range)` as
        // relative to the marked range, as NSTextInputClient does, so absolute
        // offsets would land far past the cursor.
        handler.replace_and_mark_text_in_range(None, new_text, Some(new_len..new_len));
        start
    });
    let Some(start) = start else {
        log::debug!("ime: set_composition dropped (no input handler)");
        return;
    };
    let mut state = window_ptr.state.borrow_mut();
    state.ime_composition_start = Some(start);
    state.ime_composition_text = Some(new_text.to_string());
}

/// Finalize the active composition. Behavior diverges based on what
/// kind of `InputHandler` is on the other side:
///
/// - **Editor-style** (`Editor`): the composing text is already
///   inserted into the buffer (with a marked range / underline).
///   Finalizing means `unmark_text` — drop the underline, leave the
///   characters. A `commitText` with different text replaces the
///   marked range; with the same text it's a text-level no-op that
///   only clears the mark.
///
/// - **Terminal-style** (`TerminalInputHandler`): the composing text
///   lives only in `terminal_view`'s composition overlay, NOT in the
///   PTY. `unmark_text` clears the overlay without sending to the
///   PTY. So we must explicitly call `replace_text_in_range(None,
///   text)` to deliver the finalized text. Skipping this leaves the
///   user with a phantom composition that never reaches the shell.
///
/// We distinguish via a probe before any mutating call:
/// `text_for_range(marked_range)` returns the marked content for
/// editors (because the text is in the buffer) and `None` for
/// terminals (whose handler stubs `text_for_range`). That signal
/// drives whether the finalization path needs an explicit commit.
pub(crate) fn commit_composition(
    window_ptr: &AndroidWindowStatePtr,
    replacement_text: Option<&str>,
) {
    let (previous_start, previous_text) = {
        let state = window_ptr.state.borrow();
        (
            state.ime_composition_start,
            state.ime_composition_text.clone(),
        )
    };
    let handled = window_ptr.with_input_handler(|handler| match replacement_text {
        // Both handlers replace the marked range, drop the mark, and in the
        // terminal's case send to the PTY. Calling `unmark_text` first would
        // leave the editor nothing to replace.
        Some(text) => handler.replace_text_in_range(None, text),
        None => {
            let Some(text) = previous_text.as_deref() else {
                return;
            };
            let marked_is_in_buffer = previous_start.is_some_and(|start| {
                let end = start + text.encode_utf16().count();
                let mut adjusted = None;
                handler.text_for_range(start..end, &mut adjusted).as_deref() == Some(text)
            });
            if marked_is_in_buffer {
                handler.unmark_text();
            } else {
                handler.replace_text_in_range(None, text);
            }
        }
    });
    if handled.is_none() {
        log::debug!("ime: commit_composition dropped (no input handler)");
        return;
    }
    let mut state = window_ptr.state.borrow_mut();
    state.ime_composition_start = None;
    state.ime_composition_text = None;
}

/// Stop tracking the composition and leave its text alone, for a key press whose
/// text is about to replace it.
pub(crate) fn forget_composition(window_ptr: &AndroidWindowStatePtr) {
    let mut state = window_ptr.state.borrow_mut();
    state.ime_composition_start = None;
    state.ime_composition_text = None;
}

/// Push the current editor text + selection state across to Kotlin
/// so the IME's `getTextBeforeCursor` / `getTextAfterCursor` /
/// `getSelectedText` / `getExtractedText` queries can return real
/// values, and so `InputMethodManager.updateSelection` fires.
///
/// Without this, the IME's internal state-model drifts from the
/// editor's actual state. Gboard reacts to drift by re-confirming —
/// it re-sends `setComposingText` 100-200ms after the first call
/// with the same text, defensively trying to force its model back
/// into sync. We see this as duplicate letters (`hh`, `ee`) in the
/// editor. With the mirror in place Gboard's queries return what it
/// expects and the duplicates stop.
///
/// Window size: we mirror up to ~256 chars on each side of the
/// cursor. IMEs never request more (Gboard typically asks for 50,
/// Swiftkey for 100). Sending the whole document on every keystroke
/// would be wasteful for large Zed buffers.
const IME_MIRROR_WINDOW: usize = 256;

pub(crate) fn notify_text_state(window_ptr: &AndroidWindowStatePtr) {
    let (android_app, extra_window_id, composition) = {
        let state = window_ptr.state.borrow();
        let composition = state.ime_composition_start.zip(
            state
                .ime_composition_text
                .as_ref()
                .map(|text| text.encode_utf16().count()),
        );
        (
            state.android_app.clone(),
            state.extra_window_id,
            composition,
        )
    };
    let (comp_start_i32, comp_end_i32) = composition.map_or((-1, -1), |(start, len)| {
        (start as i32, (start + len) as i32)
    });
    let mirrored = window_ptr.with_input_handler(|handler| {
        let selection = handler.selected_text_range(false)?;
        let window_start = selection.range.start.saturating_sub(IME_MIRROR_WINDOW);
        let window_end = selection.range.end.saturating_add(IME_MIRROR_WINDOW);
        let mut adjusted = None;
        let text = handler
            .text_for_range(window_start..window_end, &mut adjusted)
            .unwrap_or_default();
        let actual_window_start = adjusted.map_or(window_start, |range| range.start);
        Some((selection.range, text, actual_window_start))
    });
    let Some(Some((selection, text, actual_window_start))) = mirrored else {
        return;
    };
    let (sel_start, sel_end) = (selection.start, selection.end);

    let result = crate::jni_env::with_env(&android_app, |env| {
        let text = env.new_string(text).context("new_string for IME text")?;
        crate::jni_env::call_window_method(
            &android_app,
            extra_window_id,
            "updateImeTextState",
            "(Ljava/lang/String;IIIII)V",
            &[
                (&text).into(),
                (actual_window_start as i32).into(),
                (sel_start as i32).into(),
                (sel_end as i32).into(),
                comp_start_i32.into(),
                comp_end_i32.into(),
            ],
        )
    });
    if let Err(err) = result {
        log::warn!("ime::notify_text_state failed: {err:#}");
    }
}

fn apply_event(window_ptr: &AndroidWindowStatePtr, event: ImeEvent) {
    log::debug!("ime::apply_event {:?}", debug_event(&event));
    let needs_mirror_push = match event {
        ImeEvent::CommitText { text, .. } => {
            commit_composition(window_ptr, Some(&text));
            true
        }
        ImeEvent::SetComposingText { text, .. } => {
            set_composition(window_ptr, &text);
            true
        }
        ImeEvent::FinishComposingText => {
            // Only push mirror state if there was an active composition
            // to clear. Without this guard, Gboard fires
            // `finishComposingText` defensively after each
            // `IMM.updateSelection` push, and our unconditional
            // mirror-push echoes another updateSelection right back —
            // forming a feedback loop that floods the wire at ~16Hz
            // even when the user is idle.
            let had_composition = window_ptr.state.borrow().ime_composition_text.is_some();
            commit_composition(window_ptr, None);
            had_composition
        }
        ImeEvent::DeleteSurroundingText {
            before_length,
            after_length,
        } => {
            let deleted = window_ptr.with_input_handler(|handler| {
                let Some(selection) = handler.selected_text_range(false) else {
                    return false;
                };
                let before = (before_length.max(0) as usize).min(selection.range.start);
                let after = after_length.max(0) as usize;
                // The selection stays: the IME names what's around it. The
                // later range goes first, so the earlier one's offsets hold.
                if after > 0 {
                    let end = selection.range.end;
                    handler.replace_text_in_range(Some(end..end + after), "");
                }
                if before > 0 {
                    let start = selection.range.start;
                    handler.replace_text_in_range(Some(start - before..start), "");
                }
                true
            });
            if deleted != Some(true) {
                return;
            }
            true
        }
        ImeEvent::KeyEvent {
            action,
            keycode,
            meta_state,
            repeat_count,
        } => {
            if let Some(input) = crate::events::translate_extra_key_event(
                window_ptr,
                action,
                VIRTUAL_KEYBOARD,
                keycode,
                meta_state,
                repeat_count,
            ) {
                window_ptr.handle_input(input);
            }
            // KeyEvent may have moved the cursor (arrows) or changed
            // text (Enter, Backspace through the hardware path);
            // refresh the mirror unconditionally so the IME sees the
            // post-key state.
            true
        }
        ImeEvent::EditorAction { action_id } => {
            // Soft keyboards deliver Enter either as
            // sendKeyEvent(KEYCODE_ENTER) (handled in the KeyEvent arm) or
            // as performEditorAction, depending on the keyboard and the
            // field's imeOptions. The latter used to be dropped, so Enter
            // did nothing on keyboards / contexts that use it (single-line
            // search / finder fields without MULTI_LINE, or some IMEs in
            // the terminal). Synthesize an Enter keypress so it works
            // regardless of how the IME delivers it; gpui routes the Enter
            // to the focused element (newline in the editor, run in the
            // terminal, confirm in a picker).
            log::debug!("ime::editor_action {action_id} -> Enter");
            const ACTION_DOWN: i32 = 0;
            const ACTION_UP: i32 = 1;
            const KEYCODE_ENTER: u32 = 66;
            for key_action in [ACTION_DOWN, ACTION_UP] {
                if let Some(input) = crate::events::translate_extra_key_event(
                    window_ptr,
                    key_action,
                    VIRTUAL_KEYBOARD,
                    KEYCODE_ENTER,
                    0,
                    0,
                ) {
                    window_ptr.handle_input(input);
                }
            }
            true
        }
    };

    if needs_mirror_push {
        notify_text_state(window_ptr);
    }
}

/// Shows or hides the window's soft keyboard. gpui's
/// `request_virtual_keyboard` and `dismiss_virtual_keyboard` lead here, and a
/// text field losing focus or a physical keyboard connecting hides it.
pub(crate) fn request_keyboard(android_app: &AndroidApp, extra_window_id: Option<u64>, show: bool) {
    log::info!("ime::request_keyboard w={extra_window_id:?} show={show}");
    let result = crate::jni_env::call_window_method(
        android_app,
        extra_window_id,
        "requestIme",
        "(Z)V",
        &[show.into()],
    );
    if let Err(err) = result {
        log::warn!("ime::request_keyboard failed: {err:#}");
    }
}

// --------------------------------------------------------------------
// JNI entry points called from `ZdroidInputConnection` on the UI
// thread. Each pushes an `ImeEvent` onto the channel; the game thread
// drains.
// --------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeImeCommitText<'local>(
    mut env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    text: JString<'local>,
    new_cursor_position: i32,
) {
    let text: String = env.get_string(&text).map(|s| s.into()).unwrap_or_default();
    dispatch_event(
        window_id as u64,
        ImeEvent::CommitText {
            text,
            new_cursor_position,
        },
    );
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeImeSetComposingText<'local>(
    mut env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    text: JString<'local>,
    new_cursor_position: i32,
) {
    let text: String = env.get_string(&text).map(|s| s.into()).unwrap_or_default();
    dispatch_event(
        window_id as u64,
        ImeEvent::SetComposingText {
            text,
            new_cursor_position,
        },
    );
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeImeFinishComposingText<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) {
    dispatch_event(window_id as u64, ImeEvent::FinishComposingText);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeImeDeleteSurroundingText<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    before_length: i32,
    after_length: i32,
) {
    dispatch_event(
        window_id as u64,
        ImeEvent::DeleteSurroundingText {
            before_length,
            after_length,
        },
    );
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeImeSendKeyEvent<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    action: i32,
    keycode: i32,
    meta_state: i32,
    repeat_count: i32,
) {
    dispatch_event(
        window_id as u64,
        ImeEvent::KeyEvent {
            action,
            keycode: keycode as u32,
            meta_state: meta_state as u32,
            repeat_count,
        },
    );
}

/// Whether soft-keyboard input for this IME window should be delivered as
/// key events instead of committed as text; see [`TEXT_INPUT_FOCUSED`].
/// Read by Kotlin's `commitText`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeImeRouteAsKeys<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
) -> bool {
    route_as_keys(window_id as u64)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeImePerformEditorAction<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    window_id: i64,
    action_id: i32,
) {
    dispatch_event(window_id as u64, ImeEvent::EditorAction { action_id });
}

/// `WindowIme.kt` pushes whether the keyboard is showing here whenever it changes
/// (after showSoftInput / hideSoftInput / WindowInsetsListener edge).
/// We mirror it into [`SOFT_KEYBOARD_VISIBLE`] so the status bar keyboard button can
/// reflect the actual OS visibility.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeSetSoftKeyboardVisible<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    visible: jni::sys::jboolean,
) {
    SOFT_KEYBOARD_VISIBLE.store(visible != 0, Ordering::Release);
    crate::platform::wake_run_loop();
}

/// `MainActivity` pushes [`PHYSICAL_KEYBOARD_BLOCKS_IME`] here at startup
/// and whenever an input device or the Android setting changes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_zdroid_NativeBridge_nativeSetPhysicalKeyboardBlocksIme<'local>(
    _env: JNIEnv<'local>,
    _bridge: JObject<'local>,
    blocks_ime: jni::sys::jboolean,
) {
    PHYSICAL_KEYBOARD_BLOCKS_IME.store(blocks_ime != 0, Ordering::Release);
    crate::platform::wake_run_loop();
}
