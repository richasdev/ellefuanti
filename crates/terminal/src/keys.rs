//! Turning key presses into the bytes a shell expects.
//!
//! This lives in the domain crate, not the view, because it is a lookup table with real
//! rules (control characters are arithmetic on the letter; arrows are escape sequences)
//! and it is worth testing without a window. The app crate only decides *which* key was
//! pressed; what that key means on the wire is decided here.

/// A key press, described independently of any UI toolkit.
///
/// The app crate maps gpui's `Keystroke` onto this. Deliberately not a gpui type: that is
/// the dependency ADR-0004 forbids, and it keeps this table testable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Key {
    /// A literal character the layout produced, already accounting for shift.
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Modifiers that change the bytes sent.
///
/// No `shift`: shift is already baked into [`Key::Char`] by the layout, and for the
/// special keys below it does not change the sequence in a way this terminal supports yet.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Modifiers {
    pub control: bool,
    /// Sent as ESC-prefix (the "meta sends escape" convention every shell assumes).
    pub alt: bool,
}

impl Modifiers {
    pub const NONE: Self = Self { control: false, alt: false };

    pub const fn control() -> Self {
        Self { control: true, alt: false }
    }

    pub const fn alt() -> Self {
        Self { control: false, alt: true }
    }
}

/// Encodes a key press as PTY input bytes, or `None` if it sends nothing.
///
/// Returns owned bytes because a control character, an escape sequence and a multi-byte
/// UTF-8 char have three different lengths; a `&'static [u8]` would not cover `Char`.
pub fn encode(key: &Key, modifiers: Modifiers) -> Option<Vec<u8>> {
    let mut bytes = match key {
        Key::Char(c) => encode_char(*c, modifiers)?,
        // CR, not LF: the PTY is in canonical mode with ICRNL, and a bare LF leaves
        // shells like bash waiting for a line that never ends.
        Key::Enter => vec![b'\r'],
        // DEL (0x7f), not BS (0x08). Every Unix shell's erase character is DEL; sending
        // BS makes backspace print ^H instead of deleting.
        Key::Backspace => vec![0x7f],
        Key::Tab => vec![b'\t'],
        Key::Escape => vec![0x1b],
        Key::Delete => b"\x1b[3~".to_vec(),

        // Arrows and navigation in "normal" (cursor) mode. Application mode (ESC O A)
        // differs, and a full implementation reads the terminal's DECCKM flag.
        // ponytail: alacritty tracks DECCKM in `Term::mode()`; thread it through here
        // when an app that depends on it (readline in vi mode, htop) misbehaves.
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::Right => b"\x1b[C".to_vec(),
        Key::Left => b"\x1b[D".to_vec(),
        Key::Home => b"\x1b[H".to_vec(),
        Key::End => b"\x1b[F".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
    };

    // Alt on a non-character key prefixes the whole sequence with ESC. For characters
    // encode_char has already done it, so this only applies to the special keys.
    if modifiers.alt && !matches!(key, Key::Char(_)) {
        bytes.insert(0, 0x1b);
    }

    Some(bytes)
}

/// Encodes a literal character, applying control and alt.
fn encode_char(c: char, modifiers: Modifiers) -> Option<Vec<u8>> {
    let mut bytes = if modifiers.control {
        vec![control_byte(c)?]
    } else {
        let mut buffer = [0u8; 4];
        c.encode_utf8(&mut buffer).as_bytes().to_vec()
    };

    if modifiers.alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// The control character for a key, e.g. `c` -> 0x03 (SIGINT).
///
/// Control codes are the low 5 bits of the uppercased letter, which is why this is
/// arithmetic rather than a 26-entry table. The punctuation cases do not follow that rule
/// and are listed explicitly.
fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        // ctrl-space and ctrl-@ both send NUL.
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '?' => Some(0x1f),
        // Anything else with ctrl held sends nothing, rather than the bare character —
        // ctrl-1 must not type "1".
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_characters_are_utf8() {
        assert_eq!(encode(&Key::Char('a'), Modifiers::NONE).unwrap(), b"a");
        // Multi-byte, because a Portuguese-language project types these constantly.
        assert_eq!(encode(&Key::Char('ç'), Modifiers::NONE).unwrap(), "ç".as_bytes());
        assert_eq!(encode(&Key::Char('日'), Modifiers::NONE).unwrap(), "日".as_bytes());
    }

    #[test]
    fn enter_sends_carriage_return_not_newline() {
        // A bare LF hangs bash waiting for the rest of the line.
        assert_eq!(encode(&Key::Enter, Modifiers::NONE).unwrap(), b"\r");
    }

    #[test]
    fn backspace_sends_del_not_backspace() {
        // BS (0x08) makes shells print ^H instead of erasing.
        assert_eq!(encode(&Key::Backspace, Modifiers::NONE).unwrap(), &[0x7f]);
    }

    #[test]
    fn control_letters_are_the_low_five_bits() {
        assert_eq!(encode(&Key::Char('c'), Modifiers::control()).unwrap(), &[0x03]);
        assert_eq!(encode(&Key::Char('d'), Modifiers::control()).unwrap(), &[0x04]);
        // Case-insensitive: ctrl-shift-C is still SIGINT.
        assert_eq!(encode(&Key::Char('C'), Modifiers::control()).unwrap(), &[0x03]);
        assert_eq!(encode(&Key::Char('a'), Modifiers::control()).unwrap(), &[0x01]);
        assert_eq!(encode(&Key::Char('z'), Modifiers::control()).unwrap(), &[0x1a]);
    }

    #[test]
    fn control_punctuation_and_space() {
        assert_eq!(encode(&Key::Char(' '), Modifiers::control()).unwrap(), &[0x00]);
        assert_eq!(encode(&Key::Char('['), Modifiers::control()).unwrap(), &[0x1b]);
        assert_eq!(encode(&Key::Char('\\'), Modifiers::control()).unwrap(), &[0x1c]);
    }

    #[test]
    fn control_with_an_unmapped_key_sends_nothing() {
        // Must not fall through to typing the bare digit.
        assert!(encode(&Key::Char('1'), Modifiers::control()).is_none());
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(encode(&Key::Char('b'), Modifiers::alt()).unwrap(), &[0x1b, b'b']);
        // On a special key the ESC goes before the whole sequence, not inside it.
        assert_eq!(encode(&Key::Left, Modifiers::alt()).unwrap(), b"\x1b\x1b[D");
    }

    #[test]
    fn arrows_and_navigation_use_normal_cursor_mode() {
        assert_eq!(encode(&Key::Up, Modifiers::NONE).unwrap(), b"\x1b[A");
        assert_eq!(encode(&Key::Down, Modifiers::NONE).unwrap(), b"\x1b[B");
        assert_eq!(encode(&Key::Right, Modifiers::NONE).unwrap(), b"\x1b[C");
        assert_eq!(encode(&Key::Left, Modifiers::NONE).unwrap(), b"\x1b[D");
        assert_eq!(encode(&Key::Delete, Modifiers::NONE).unwrap(), b"\x1b[3~");
        assert_eq!(encode(&Key::PageUp, Modifiers::NONE).unwrap(), b"\x1b[5~");
    }
}
