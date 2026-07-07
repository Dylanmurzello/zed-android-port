//! Keyboard input translation: Android `KeyEvent` (and the raw-fields
//! variant used by the ExtraWindowActivity JNI bridge) → gpui
//! `PlatformInput::KeyDown` / `KeyUp` / `ModifiersChanged`.
//!
//! Hardware keys only. IME / soft-keyboard composition lives in `ime.rs`
//! when that lands (see `deferred-soft-keyboard.md`).

use android_activity::input::{KeyAction, KeyEvent, Keycode, MetaState};
use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, PlatformInput,
};

/// Convert an Android `KeyEvent` into a gpui `PlatformInput`.
///
/// Returns `None` when the event isn't translatable (e.g. `KeyAction::Multiple`,
/// which is reserved for synthesized character sequences from soft keyboards we
/// don't currently support).
pub(crate) fn translate_key_event(event: &KeyEvent) -> Option<PlatformInput> {
    let action = event.action();
    let keycode = event.key_code();
    let modifiers = modifiers_from_meta(event.meta_state());

    if is_modifier_key(keycode) {
        return Some(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            capslock: capslock_from_meta(event.meta_state()),
        }));
    }

    let keystroke = build_keystroke(keycode, modifiers);

    match action {
        KeyAction::Down => Some(PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: event.repeat_count() > 0,
            prefer_character_input: false,
        })),
        KeyAction::Up => Some(PlatformInput::KeyUp(KeyUpEvent { keystroke })),
        _ => None,
    }
}

/// Same shape as [`translate_key_event`] but takes raw Android KeyEvent
/// fields instead of an `android_activity::KeyEvent` object. Used by the
/// ExtraWindowActivity JNI bridge (`multi_window::nativeOnExtraKeyEvent`)
/// which sees `MotionEvent`/`KeyEvent` via Java reflection: those Java
/// objects can't be reconstructed into `android_activity::KeyEvent` on
/// the Rust side, so we accept the primitive fields and rebuild the
/// translation pipeline on top of them.
///
/// `action`: `KeyEvent.ACTION_DOWN` (0) or `ACTION_UP` (1).
/// `keycode_raw`: Android `KeyEvent.getKeyCode()` (AKEYCODE_*).
/// `meta_state_raw`: Android `KeyEvent.getMetaState()` (META_* bitfield).
/// `repeat_count`: `KeyEvent.getRepeatCount()` for auto-repeat detection.
pub(crate) fn translate_extra_key_event(
    action: i32,
    keycode_raw: u32,
    meta_state_raw: u32,
    repeat_count: i32,
) -> Option<PlatformInput> {
    let meta = MetaState(meta_state_raw);
    let keycode = Keycode::from(keycode_raw);
    let modifiers = modifiers_from_meta(meta);

    if is_modifier_key(keycode) {
        return Some(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            capslock: capslock_from_meta(meta),
        }));
    }

    let keystroke = build_keystroke(keycode, modifiers);

    // Android KeyEvent.ACTION_DOWN = 0, ACTION_UP = 1, ACTION_MULTIPLE = 2.
    // Same translation policy as `translate_key_event`: only Down/Up
    // produce inputs; Multiple is reserved for synthesized soft-keyboard
    // char sequences we don't currently support.
    match action {
        0 => Some(PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: repeat_count > 0,
            prefer_character_input: false,
        })),
        1 => Some(PlatformInput::KeyUp(KeyUpEvent { keystroke })),
        _ => None,
    }
}

pub(crate) fn modifiers_from_meta(meta: MetaState) -> Modifiers {
    Modifiers {
        shift: meta.shift_on(),
        control: meta.ctrl_on(),
        alt: meta.alt_on(),
        platform: meta.meta_on(),
        function: meta.function_on(),
    }
}

fn capslock_from_meta(meta: MetaState) -> Capslock {
    Capslock {
        on: meta.caps_lock_on(),
    }
}

fn is_modifier_key(code: Keycode) -> bool {
    use Keycode::*;
    matches!(
        code,
        ShiftLeft | ShiftRight | AltLeft | AltRight | CtrlLeft | CtrlRight | MetaLeft | MetaRight
    )
}

fn build_keystroke(code: Keycode, mut modifiers: Modifiers) -> Keystroke {
    let (key, key_char) = if let Some(named) = named_key(code) {
        // Space is the one named key where gpui still wants a printable
        // key_char so text-input paths can insert " ".
        let key_char = matches!(code, Keycode::Space).then(|| " ".to_string());
        (named.to_string(), key_char)
    } else if let Some(ch) = lowercased_key(code) {
        let typed = if modifiers.shift {
            apply_shift(ch)
        } else {
            ch
        };
        // X11 resolves `key` through the shift level: shift-8 IS "*". Leaving
        // `key` as the unshifted char makes symbol bindings unmatchable and
        // collides with the digit's own binding (vim's "*" search lost to
        // "8" = vim::Number, which wins on later-added precedence). Letters
        // keep the lowercase key; their shift survives the drop below.
        let key = if typed.is_ascii_alphabetic() { ch } else { typed };
        (key.to_string(), Some(typed.to_string()))
    } else {
        (format!("{code:?}").to_lowercase(), None)
    };

    // Drop the shift modifier for non-alpha single-char keys: the shifted
    // value is already in `key_char` and bindings like `shift-1` should match
    // as `!`. Mirrors X11's `keystroke_from_xkb` behavior.
    if modifiers.shift
        && key.chars().count() == 1
        && key.chars().next().map_or(false, |c| {
            c.to_lowercase().to_string() == c.to_uppercase().to_string()
        })
    {
        modifiers.shift = false;
    }

    Keystroke {
        modifiers,
        key,
        key_char,
    }
}

fn named_key(code: Keycode) -> Option<&'static str> {
    use Keycode::*;
    Some(match code {
        Enter | NumpadEnter => "enter",
        Tab => "tab",
        Space => "space",
        Del => "backspace",
        ForwardDel => "delete",
        Escape => "escape",
        DpadUp => "up",
        DpadDown => "down",
        DpadLeft => "left",
        DpadRight => "right",
        MoveHome => "home",
        MoveEnd => "end",
        PageUp => "pageup",
        PageDown => "pagedown",
        Insert => "insert",
        F1 => "f1",
        F2 => "f2",
        F3 => "f3",
        F4 => "f4",
        F5 => "f5",
        F6 => "f6",
        F7 => "f7",
        F8 => "f8",
        F9 => "f9",
        F10 => "f10",
        F11 => "f11",
        F12 => "f12",
        _ => return None,
    })
}

fn lowercased_key(code: Keycode) -> Option<char> {
    use Keycode::*;
    Some(match code {
        A => 'a',
        B => 'b',
        C => 'c',
        D => 'd',
        E => 'e',
        F => 'f',
        G => 'g',
        H => 'h',
        I => 'i',
        J => 'j',
        K => 'k',
        L => 'l',
        M => 'm',
        N => 'n',
        O => 'o',
        P => 'p',
        Q => 'q',
        R => 'r',
        S => 's',
        T => 't',
        U => 'u',
        V => 'v',
        W => 'w',
        X => 'x',
        Y => 'y',
        Z => 'z',
        Keycode0 => '0',
        Keycode1 => '1',
        Keycode2 => '2',
        Keycode3 => '3',
        Keycode4 => '4',
        Keycode5 => '5',
        Keycode6 => '6',
        Keycode7 => '7',
        Keycode8 => '8',
        Keycode9 => '9',
        Period => '.',
        Comma => ',',
        Slash => '/',
        Backslash => '\\',
        Semicolon => ';',
        Apostrophe => '\'',
        Grave => '`',
        Minus => '-',
        Equals => '=',
        LeftBracket => '[',
        RightBracket => ']',
        _ => return None,
    })
}

fn apply_shift(ch: char) -> char {
    match ch {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        _ => ch.to_ascii_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTION_DOWN: i32 = 0;
    const KEYCODE_8: u32 = 15;
    const KEYCODE_A: u32 = 29;
    const KEYCODE_TAB: u32 = 61;
    const KEYCODE_MINUS: u32 = 69;
    const META_SHIFT: u32 = 0x41; // META_SHIFT_ON | META_SHIFT_LEFT_ON
    const META_CTRL: u32 = 0x3000; // META_CTRL_ON | META_CTRL_LEFT_ON

    fn key_down(keycode: u32, meta: u32) -> Keystroke {
        match translate_extra_key_event(ACTION_DOWN, keycode, meta, 0) {
            Some(PlatformInput::KeyDown(event)) => event.keystroke,
            other => panic!("expected KeyDown, got {other:?}"),
        }
    }

    #[test]
    fn shifted_symbol_resolves_to_symbol_key() {
        let keystroke = key_down(KEYCODE_8, META_SHIFT);
        assert_eq!(keystroke.key, "*");
        assert_eq!(keystroke.key_char.as_deref(), Some("*"));
        assert_eq!(keystroke.modifiers, Modifiers::default());
    }

    #[test]
    fn unshifted_digit_stays_digit() {
        let keystroke = key_down(KEYCODE_8, 0);
        assert_eq!(keystroke.key, "8");
        assert_eq!(keystroke.key_char.as_deref(), Some("8"));
        assert_eq!(keystroke.modifiers, Modifiers::default());
    }

    #[test]
    fn shifted_letter_keeps_lowercase_key_and_shift() {
        let keystroke = key_down(KEYCODE_A, META_SHIFT);
        assert_eq!(keystroke.key, "a");
        assert_eq!(keystroke.key_char.as_deref(), Some("A"));
        assert!(keystroke.modifiers.shift);
    }

    #[test]
    fn ctrl_survives_shift_symbol_resolution() {
        let keystroke = key_down(KEYCODE_MINUS, META_SHIFT | META_CTRL);
        assert_eq!(keystroke.key, "_");
        assert!(keystroke.modifiers.control);
        assert!(!keystroke.modifiers.shift);
    }

    #[test]
    fn shifted_named_key_keeps_shift() {
        let keystroke = key_down(KEYCODE_TAB, META_SHIFT);
        assert_eq!(keystroke.key, "tab");
        assert!(keystroke.modifiers.shift);
    }
}
