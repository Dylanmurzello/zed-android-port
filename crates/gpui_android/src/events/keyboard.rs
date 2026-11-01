//! Keyboard input translation: Android `KeyEvent` (and the raw-fields
//! variant used by the ExtraWindowActivity JNI bridge) → gpui
//! `PlatformInput::KeyDown` / `KeyUp` / `ModifiersChanged`.
//!
//! A key types what the keyboard's layout in Android's settings says. Its name
//! for shortcuts follows Zed's Linux backend: a character in ASCII names the key,
//! and any other leaves it named by its keycode on a US layout, so on a Turkish
//! layout `ğ` types `ğ` and Ctrl+`ğ` is still `ctrl-[`.

use android_activity::input::{KeyAction, KeyEvent, Keycode, MetaState};
use anyhow::Result;
use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, PlatformInput,
};

use crate::window::AndroidWindowStatePtr;

/// `KeyCharacterMap.COMBINING_ACCENT`: the bit that marks a dead key.
const COMBINING_ACCENT: u32 = 0x8000_0000;
/// `KeyEvent.META_ALT_ON`.
const META_ALT_ON: u32 = 0x02;
/// `KeyEvent.META_ALT_RIGHT_ON`: AltGr on the layouts that define it.
const META_ALT_RIGHT_ON: u32 = 0x20;
/// The `KeyEvent.META_*` bits for shift, caps lock and num lock. Ctrl and the
/// like are left out of a layout lookup: the map has no character for them,
/// and a shortcut still needs to know what the key types.
const META_CHARACTER_MASK: u32 = 0x01 | 0x40 | 0x80 | 0x10_0000 | 0x20_0000;

/// What a key press types on the keyboard's layout. The default is what a key
/// without a character types, or one whose layout couldn't be read, which then
/// falls back to the US layout.
#[derive(Default)]
struct Typed {
    /// The key's own character.
    character: Option<char>,
    /// The text it inserts: `character`, or what that makes with a dead key.
    text: Option<String>,
    /// Whether the character needed AltGr, which then isn't a modifier for
    /// shortcuts or for gpui's check that text may be inserted.
    used_alt_gr: bool,
}

/// Convert an Android `KeyEvent` into a gpui `PlatformInput`.
///
/// Returns `None` when the event isn't translatable (e.g. `KeyAction::Multiple`,
/// which is reserved for synthesized character sequences from soft keyboards we
/// don't currently support).
pub(crate) fn translate_key_event(
    window: &AndroidWindowStatePtr,
    event: &KeyEvent,
) -> Option<PlatformInput> {
    let is_down = match event.action() {
        KeyAction::Down => true,
        KeyAction::Up => false,
        _ => return None,
    };
    translate_key(
        window,
        is_down,
        event.device_id(),
        event.key_code(),
        event.meta_state(),
        event.repeat_count(),
    )
}

/// Same shape as [`translate_key_event`] but takes raw Android KeyEvent
/// fields instead of an `android_activity::KeyEvent` object. Used by the
/// ExtraWindowActivity JNI bridge (`multi_window::nativeOnExtraKeyEvent`)
/// which sees `MotionEvent`/`KeyEvent` via Java reflection: those Java
/// objects can't be reconstructed into `android_activity::KeyEvent` on
/// the Rust side, so we accept the primitive fields and rebuild the
/// translation pipeline on top of them.
///
/// `action`: `KeyEvent.ACTION_DOWN` (0) or `ACTION_UP` (1). The others, such
/// as `ACTION_MULTIPLE`, produce no input.
/// `device_id`: `KeyEvent.getDeviceId()`, which selects the layout.
/// `keycode_raw`: Android `KeyEvent.getKeyCode()` (AKEYCODE_*).
/// `meta_state_raw`: Android `KeyEvent.getMetaState()` (META_* bitfield).
/// `repeat_count`: `KeyEvent.getRepeatCount()` for auto-repeat detection.
pub(crate) fn translate_extra_key_event(
    window: &AndroidWindowStatePtr,
    action: i32,
    device_id: i32,
    keycode_raw: u32,
    meta_state_raw: u32,
    repeat_count: i32,
) -> Option<PlatformInput> {
    let is_down = match action {
        0 => true,
        1 => false,
        _ => return None,
    };
    translate_key(
        window,
        is_down,
        device_id,
        Keycode::from(keycode_raw),
        MetaState(meta_state_raw),
        repeat_count,
    )
}

fn translate_key(
    window: &AndroidWindowStatePtr,
    is_down: bool,
    device_id: i32,
    keycode: Keycode,
    meta: MetaState,
    repeat_count: i32,
) -> Option<PlatformInput> {
    let modifiers = modifiers_from_meta(meta);
    let modifiers_changed = PlatformInput::ModifiersChanged(ModifiersChangedEvent {
        modifiers,
        capslock: capslock_from_meta(meta),
    });

    if is_modifier_key(keycode) {
        return Some(modifiers_changed);
    }

    // A dead key only shows its accent until the next key. Reporting no change,
    // rather than no input, keeps Android's fallback handling from acting on it.
    let Some(typed) = typed_by_layout(
        window,
        is_down,
        repeat_count,
        device_id,
        keycode,
        meta,
        &modifiers,
    ) else {
        return Some(modifiers_changed);
    };
    let keystroke = build_keystroke(keycode, modifiers, typed);

    if is_down {
        Some(PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: repeat_count > 0,
            prefer_character_input: false,
        }))
    } else {
        Some(PlatformInput::KeyUp(KeyUpEvent { keystroke }))
    }
}

/// Looks up what a key types, combining it with a pending dead key on a press.
/// Returns `None` for a press that only leaves an accent for the next key.
///
/// Until then the accent shows as composing text, as it does in Android's own
/// text fields (`QwertyKeyListener`) and in Zed on Linux. A key that can't
/// combine with it leaves it in the text, as both do.
fn typed_by_layout(
    window: &AndroidWindowStatePtr,
    is_down: bool,
    repeat_count: i32,
    device_id: i32,
    code: Keycode,
    meta: MetaState,
    modifiers: &Modifiers,
) -> Option<Typed> {
    let is_shortcut = modifiers.control || modifiers.platform;
    let is_named = named_key(code).is_some();
    let pending = if is_down {
        take_pending_accent(window)
    } else {
        None
    };
    // Named keys have no character to look up, apart from a space finishing a
    // dead key, which types the accent itself.
    let accent = pending.filter(|_| !is_shortcut && (!is_named || matches!(code, Keycode::Space)));
    if pending.is_some() && accent.is_none() {
        crate::ime::commit_composition(window, None);
    }
    if is_named && accent.is_none() {
        return Some(Typed::default());
    }

    let (raw, used_alt_gr) = match layout_character(device_id, code, meta) {
        Ok(found) => found,
        Err(error) => {
            log::warn!("keyboard: couldn't read the keyboard's layout: {error:#}");
            if accent.is_some() {
                crate::ime::commit_composition(window, None);
            }
            return Some(Typed::default());
        }
    };

    if raw & COMBINING_ACCENT != 0 {
        if !is_down || is_shortcut {
            return Some(Typed::default());
        }
        // Holding a dead key changes nothing, as in `QwertyKeyListener`.
        if repeat_count > 0 {
            window.state.borrow_mut().pending_accent = accent;
            return None;
        }
        let dead = char::from_u32(raw & !COMBINING_ACCENT).map(typable_accent);
        let Some(accent) = accent else {
            wait_for_next_key(window, dead);
            return None;
        };
        // Pressing a dead key twice types its accent, as
        // `KeyCharacterMap.getDeadChar` has it.
        if dead == Some(accent) {
            crate::ime::forget_composition(window);
            return Some(Typed {
                character: Some(accent),
                text: Some(accent.to_string()),
                used_alt_gr,
            });
        }
        // Another dead key leaves the first accent in the text and waits its turn.
        crate::ime::commit_composition(window, None);
        wait_for_next_key(window, dead);
        return None;
    }

    let Some(character) = char::from_u32(raw).filter(|character| !character.is_control()) else {
        if accent.is_some() {
            crate::ime::commit_composition(window, None);
        }
        return Some(Typed::default());
    };
    // The key's text replaces the composing accent when gpui inserts it.
    if accent.is_some() {
        crate::ime::forget_composition(window);
    }
    let text = match accent {
        Some(accent) => match dead_key_character(accent, character) {
            Ok(Some(combined)) => combined.to_string(),
            Ok(None) => format!("{accent}{character}"),
            Err(error) => {
                log::warn!("keyboard: couldn't combine a dead key: {error:#}");
                format!("{accent}{character}")
            }
        },
        None => character.to_string(),
    };
    // A space after a dead key types the accent on its own, and is that key.
    let character = match accent {
        Some(accent) if character == ' ' => accent,
        _ => character,
    };
    Some(Typed {
        character: Some(character),
        text: Some(text),
        used_alt_gr,
    })
}

/// The accent waiting in `window` for the next key. Tapping elsewhere in the text
/// abandons the composition, which leaves the accent there and types the next key
/// on its own.
fn take_pending_accent(window: &AndroidWindowStatePtr) -> Option<char> {
    let mut state = window.state.borrow_mut();
    let accent = state.pending_accent.take()?;
    let composing = state.ime_composition_text.as_deref()?;
    composing.chars().eq([accent]).then_some(accent)
}

fn wait_for_next_key(window: &AndroidWindowStatePtr, accent: Option<char>) {
    let Some(accent) = accent else {
        return;
    };
    crate::ime::set_composition(window, accent.encode_utf8(&mut [0; 4]));
    window.state.borrow_mut().pending_accent = Some(accent);
}

/// `KeyCharacterMap` reports the dead keys for `^`, `~` and `` ` `` with spacing
/// accents outside ASCII, which is what they'd then type: Android's Turkish
/// layout has all three only as dead keys. `getDeadChar` combines the ASCII
/// characters the same way.
fn typable_accent(accent: char) -> char {
    match accent {
        '\u{02C6}' => '^',
        '\u{02DC}' => '~',
        '\u{02CB}' => '`',
        other => other,
    }
}

/// The raw `KeyCharacterMap.get` result for a key, and whether AltGr produced
/// it. A layout without a character on AltGr, such as US, answers for the plain
/// key, and the key stays an Alt shortcut.
fn layout_character(device_id: i32, code: Keycode, meta: MetaState) -> Result<(u32, bool)> {
    let android_app = crate::jni_env::android_app()?;
    let code: u32 = code.into();
    crate::jni_env::with_activity(android_app, |env, activity| {
        let mut look_up = |meta_state: u32| -> Result<u32> {
            let character = env
                .call_method(
                    activity,
                    "keyCharacter",
                    "(III)I",
                    &[
                        device_id.into(),
                        (code as i32).into(),
                        (meta_state as i32).into(),
                    ],
                )?
                .i()?;
            Ok(character as u32)
        };
        let plain = meta.0 & META_CHARACTER_MASK;
        if meta.0 & META_ALT_RIGHT_ON != 0 {
            let with_alt_gr = look_up(plain | META_ALT_ON | META_ALT_RIGHT_ON)?;
            if with_alt_gr != 0 {
                return Ok((with_alt_gr, true));
            }
        }
        Ok((look_up(plain)?, false))
    })
}

fn dead_key_character(accent: char, character: char) -> Result<Option<char>> {
    let android_app = crate::jni_env::android_app()?;
    crate::jni_env::with_activity(android_app, |env, activity| {
        let combined = env
            .call_method(
                activity,
                "deadKeyCharacter",
                "(II)I",
                &[(accent as i32).into(), (character as i32).into()],
            )?
            .i()?;
        Ok(char::from_u32(combined as u32).filter(|combined| *combined != '\0'))
    })
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

fn build_keystroke(code: Keycode, mut modifiers: Modifiers, typed: Typed) -> Keystroke {
    let (key, key_char) = if let Some(named) = named_key(code).filter(|_| typed.character.is_none())
    {
        // Space is the one named key where gpui still wants a printable
        // key_char so text-input paths can insert " ".
        let key_char = matches!(code, Keycode::Space).then(|| " ".to_string());
        (named.to_string(), key_char)
    } else {
        let us_key = lowercased_key(code);
        let us_typed = us_key.map(|ch| if modifiers.shift { apply_shift(ch) } else { ch });
        // As Zed's Linux backend does, a character outside ASCII leaves the
        // key named as it is on a US layout, so that it still works in shortcuts.
        let key_name = typed
            .character
            .filter(char::is_ascii)
            .or(us_typed)
            .or(typed.character);
        match key_name {
            // X11 resolves `key` through the shift level: shift-8 IS "*". Leaving
            // `key` as the unshifted char makes symbol bindings unmatchable and
            // collides with the digit's own binding (vim's "*" search lost to
            // "8" = vim::Number, which wins on later-added precedence). Letters
            // keep the lowercase key; their shift survives the drop below.
            Some(key_name) => (
                key_name.to_lowercase().to_string(),
                typed.text.or_else(|| us_typed.map(|ch| ch.to_string())),
            ),
            None => (format!("{code:?}").to_lowercase(), None),
        }
    };

    if typed.used_alt_gr {
        modifiers.alt = false;
    }

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

    const META_SHIFT: u32 = 0x41; // META_SHIFT_ON | META_SHIFT_LEFT_ON
    const META_CTRL: u32 = 0x3000; // META_CTRL_ON | META_CTRL_LEFT_ON

    fn key_down(keycode: Keycode, meta: u32, typed: Typed) -> Keystroke {
        build_keystroke(keycode, modifiers_from_meta(MetaState(meta)), typed)
    }

    fn typing(character: char) -> Typed {
        Typed {
            character: Some(character),
            text: Some(character.to_string()),
            used_alt_gr: false,
        }
    }

    #[test]
    fn shifted_symbol_resolves_to_symbol_key() {
        for typed in [Typed::default(), typing('*')] {
            let keystroke = key_down(Keycode::Keycode8, META_SHIFT, typed);
            assert_eq!(keystroke.key, "*");
            assert_eq!(keystroke.key_char.as_deref(), Some("*"));
            assert_eq!(keystroke.modifiers, Modifiers::default());
        }
    }

    #[test]
    fn unshifted_digit_stays_digit() {
        for typed in [Typed::default(), typing('8')] {
            let keystroke = key_down(Keycode::Keycode8, 0, typed);
            assert_eq!(keystroke.key, "8");
            assert_eq!(keystroke.key_char.as_deref(), Some("8"));
            assert_eq!(keystroke.modifiers, Modifiers::default());
        }
    }

    #[test]
    fn shifted_letter_keeps_lowercase_key_and_shift() {
        for typed in [Typed::default(), typing('A')] {
            let keystroke = key_down(Keycode::A, META_SHIFT, typed);
            assert_eq!(keystroke.key, "a");
            assert_eq!(keystroke.key_char.as_deref(), Some("A"));
            assert!(keystroke.modifiers.shift);
        }
    }

    #[test]
    fn ctrl_survives_shift_symbol_resolution() {
        let keystroke = key_down(Keycode::Minus, META_SHIFT | META_CTRL, typing('_'));
        assert_eq!(keystroke.key, "_");
        assert!(keystroke.modifiers.control);
        assert!(!keystroke.modifiers.shift);
    }

    #[test]
    fn shifted_named_key_keeps_shift() {
        let keystroke = key_down(Keycode::Tab, META_SHIFT, Typed::default());
        assert_eq!(keystroke.key, "tab");
        assert!(keystroke.modifiers.shift);
    }

    #[test]
    fn letter_outside_ascii_types_itself_and_keeps_the_us_shortcut_key() {
        let keystroke = key_down(Keycode::LeftBracket, 0, typing('ğ'));
        assert_eq!(keystroke.key, "[");
        assert_eq!(keystroke.key_char.as_deref(), Some("ğ"));

        let keystroke = key_down(Keycode::LeftBracket, META_CTRL, typing('ğ'));
        assert_eq!(keystroke.key, "[");
        assert!(keystroke.modifiers.control);
    }

    #[test]
    fn layout_decides_the_key_of_ascii_letters() {
        // The key at US `'` types `i` on a Turkish layout.
        let keystroke = key_down(Keycode::Apostrophe, 0, typing('i'));
        assert_eq!(keystroke.key, "i");
        assert_eq!(keystroke.key_char.as_deref(), Some("i"));
    }

    #[test]
    fn alt_gr_character_inserts_as_plain_text() {
        const META_ALT_GR: u32 = META_ALT_ON | META_ALT_RIGHT_ON;
        let keystroke = key_down(
            Keycode::Keycode8,
            META_ALT_GR,
            Typed {
                used_alt_gr: true,
                ..typing('[')
            },
        );
        assert_eq!(keystroke.key, "[");
        assert_eq!(keystroke.key_char.as_deref(), Some("["));
        assert_eq!(keystroke.modifiers, Modifiers::default());
    }

    #[test]
    fn alt_stays_a_modifier_when_the_layout_has_no_alt_gr_character() {
        let keystroke = key_down(Keycode::A, META_ALT_ON, typing('a'));
        assert_eq!(keystroke.key, "a");
        assert!(keystroke.modifiers.alt);
    }

    #[test]
    fn dead_key_result_inserts_the_combined_text() {
        let keystroke = key_down(
            Keycode::A,
            0,
            Typed {
                text: Some("ã".to_string()),
                ..typing('a')
            },
        );
        assert_eq!(keystroke.key, "a");
        assert_eq!(keystroke.key_char.as_deref(), Some("ã"));
    }

    #[test]
    fn space_finishing_a_dead_key_is_the_accent_key() {
        let keystroke = key_down(
            Keycode::Space,
            0,
            Typed {
                text: Some("~".to_string()),
                ..typing('~')
            },
        );
        assert_eq!(keystroke.key, "~");
        assert_eq!(keystroke.key_char.as_deref(), Some("~"));
    }

    #[test]
    fn accents_with_an_ascii_form_type_it() {
        assert_eq!(typable_accent('\u{02C6}'), '^');
        assert_eq!(typable_accent('\u{02DC}'), '~');
        assert_eq!(typable_accent('\u{02CB}'), '`');
        assert_eq!(typable_accent('\u{00B4}'), '\u{00B4}');
    }

    #[test]
    fn plain_space_types_a_space() {
        let keystroke = key_down(Keycode::Space, 0, Typed::default());
        assert_eq!(keystroke.key_char.as_deref(), Some(" "));
    }
}
