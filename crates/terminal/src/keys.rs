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

/// The DEC private modes that change what a key press or a paste puts on the wire.
///
/// A plain-data copy of the two flags out of alacritty's `TermMode` this crate acts on,
/// rather than re-exporting the bitflags type: the view holds one of these across a frame,
/// and it must not be a borrow of anything behind the terminal lock.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct TermFlags {
    /// DECCKM. Set by any program that puts the cursor keys in application mode — vim,
    /// readline in vi mode, htop. The arrows must then send `ESC O A` rather than
    /// `ESC [ A`, or the program sees a literal `[` and the arrow does nothing.
    pub application_cursor: bool,
    /// DEC 2004. Set by a shell that can distinguish pasted text from typed text, which
    /// is what stops a multi-line paste from executing every line as it arrives.
    pub bracketed_paste: bool,
}

/// Encodes a key press as PTY input bytes, or `None` if it sends nothing.
///
/// Returns owned bytes because a control character, an escape sequence and a multi-byte
/// UTF-8 char have three different lengths; a `&'static [u8]` would not cover `Char`.
pub fn encode(key: &Key, modifiers: Modifiers, flags: TermFlags) -> Option<Vec<u8>> {
    // In application cursor mode the CSI introducer becomes SS3: `ESC [ A` -> `ESC O A`.
    // Only the cursor and Home/End keys change; the tilde sequences (Delete, PageUp) are
    // unaffected, which is why this is a prefix and not a second table.
    let csi: &[u8] = if flags.application_cursor { b"\x1bO" } else { b"\x1b[" };
    let sequence = |final_byte: u8| {
        let mut bytes = csi.to_vec();
        bytes.push(final_byte);
        bytes
    };

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

        Key::Up => sequence(b'A'),
        Key::Down => sequence(b'B'),
        Key::Right => sequence(b'C'),
        Key::Left => sequence(b'D'),
        Key::Home => sequence(b'H'),
        Key::End => sequence(b'F'),
        // Tilde sequences are the same in both modes.
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

/// Encodes pasted text for the PTY.
///
/// Two things happen here that a naive `write(text)` gets wrong:
///
/// 1. **Bracketed paste.** With DEC 2004 set the text is wrapped in `ESC [ 200 ~` and
///    `ESC [ 201 ~`, which tells the shell to treat every byte between them as literal
///    input. Without it, a paste containing newlines runs each line as a command the
///    moment it arrives — the classic way to lose work to a stray copy.
/// 2. **Line endings.** A `\n` from the clipboard is not what Enter sends; the PTY's
///    ICRNL expects CR. `\r\n` from a Windows-authored file collapses to one CR, or the
///    shell sees a blank line after every real one.
///
/// ESC is dropped from the text entirely. A paste containing `ESC [ 201 ~` would close
/// the bracket early and let the rest execute — the injection this whole mechanism exists
/// to prevent — and stripping the *marker* is not enough: `ESC [ 20` + `ESC [ 201 ~` + `1~`
/// survives one pass and the fragments rejoin into a real marker on the wire. Dropping the
/// introducer is the version with no splice to reason about, and nothing that belongs in a
/// paste contains a raw ESC in the first place.
pub fn encode_paste(text: &str, flags: TermFlags) -> Vec<u8> {
    let mut body = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // Collapse CRLF; a lone CR is already what the shell wants.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                body.push('\r');
            }
            '\n' => body.push('\r'),
            // Dropped in both modes: without brackets an ESC is just as capable of
            // driving the terminal from clipboard content the user never inspected.
            '\x1b' => {}
            _ => body.push(c),
        }
    }

    if !flags.bracketed_paste {
        return body.into_bytes();
    }

    let mut bytes = Vec::with_capacity(body.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(body.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
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

    /// Encoding in the default modes, which is what most of these assertions are about.
    fn plain(key: Key, modifiers: Modifiers) -> Vec<u8> {
        encode(&key, modifiers, TermFlags::default()).expect("key should encode")
    }

    #[test]
    fn plain_characters_are_utf8() {
        assert_eq!(plain(Key::Char('a'), Modifiers::NONE), b"a");
        // Multi-byte, because a Portuguese-language project types these constantly.
        assert_eq!(plain(Key::Char('\u{e7}'), Modifiers::NONE), "\u{e7}".as_bytes());
        assert_eq!(plain(Key::Char('\u{65e5}'), Modifiers::NONE), "\u{65e5}".as_bytes());
    }

    #[test]
    fn enter_sends_carriage_return_not_newline() {
        // A bare LF hangs bash waiting for the rest of the line.
        assert_eq!(plain(Key::Enter, Modifiers::NONE), b"\r");
    }

    #[test]
    fn backspace_sends_del_not_backspace() {
        // BS (0x08) makes shells print ^H instead of erasing.
        assert_eq!(plain(Key::Backspace, Modifiers::NONE), &[0x7f]);
    }

    #[test]
    fn control_letters_are_the_low_five_bits() {
        assert_eq!(plain(Key::Char('c'), Modifiers::control()), &[0x03]);
        assert_eq!(plain(Key::Char('d'), Modifiers::control()), &[0x04]);
        // Case-insensitive: ctrl-shift-C is still SIGINT.
        assert_eq!(plain(Key::Char('C'), Modifiers::control()), &[0x03]);
        assert_eq!(plain(Key::Char('a'), Modifiers::control()), &[0x01]);
        assert_eq!(plain(Key::Char('z'), Modifiers::control()), &[0x1a]);
    }

    #[test]
    fn control_punctuation_and_space() {
        assert_eq!(plain(Key::Char(' '), Modifiers::control()), &[0x00]);
        assert_eq!(plain(Key::Char('['), Modifiers::control()), &[0x1b]);
        assert_eq!(plain(Key::Char('\\'), Modifiers::control()), &[0x1c]);
    }

    #[test]
    fn control_with_an_unmapped_key_sends_nothing() {
        // Must not fall through to typing the bare digit.
        assert!(encode(&Key::Char('1'), Modifiers::control(), TermFlags::default()).is_none());
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(plain(Key::Char('b'), Modifiers::alt()), &[0x1b, b'b']);
        // On a special key the ESC goes before the whole sequence, not inside it.
        assert_eq!(plain(Key::Left, Modifiers::alt()), b"\x1b\x1b[D");
    }

    #[test]
    fn arrows_and_navigation_use_normal_cursor_mode() {
        assert_eq!(plain(Key::Up, Modifiers::NONE), b"\x1b[A");
        assert_eq!(plain(Key::Down, Modifiers::NONE), b"\x1b[B");
        assert_eq!(plain(Key::Right, Modifiers::NONE), b"\x1b[C");
        assert_eq!(plain(Key::Left, Modifiers::NONE), b"\x1b[D");
        assert_eq!(plain(Key::Delete, Modifiers::NONE), b"\x1b[3~");
        assert_eq!(plain(Key::PageUp, Modifiers::NONE), b"\x1b[5~");
    }

    /// The DECCKM bug: without this the arrows send CSI in application mode and vim sees
    /// a literal `[` instead of a cursor movement.
    #[test]
    fn application_cursor_mode_sends_ss3_not_csi() {
        let app = TermFlags { application_cursor: true, ..TermFlags::default() };

        assert_eq!(encode(&Key::Up, Modifiers::NONE, app).unwrap(), b"\x1bOA");
        assert_eq!(encode(&Key::Down, Modifiers::NONE, app).unwrap(), b"\x1bOB");
        assert_eq!(encode(&Key::Right, Modifiers::NONE, app).unwrap(), b"\x1bOC");
        assert_eq!(encode(&Key::Left, Modifiers::NONE, app).unwrap(), b"\x1bOD");
        assert_eq!(encode(&Key::Home, Modifiers::NONE, app).unwrap(), b"\x1bOH");
        assert_eq!(encode(&Key::End, Modifiers::NONE, app).unwrap(), b"\x1bOF");
    }

    #[test]
    fn application_cursor_mode_leaves_the_tilde_sequences_alone() {
        let app = TermFlags { application_cursor: true, ..TermFlags::default() };
        // These have no SS3 form; sending `ESC O 3 ~` would be nonsense.
        assert_eq!(encode(&Key::Delete, Modifiers::NONE, app).unwrap(), b"\x1b[3~");
        assert_eq!(encode(&Key::PageUp, Modifiers::NONE, app).unwrap(), b"\x1b[5~");
        assert_eq!(encode(&Key::PageDown, Modifiers::NONE, app).unwrap(), b"\x1b[6~");
        // And a plain character is unaffected either way.
        assert_eq!(encode(&Key::Char('a'), Modifiers::NONE, app).unwrap(), b"a");
    }

    #[test]
    fn a_plain_paste_becomes_carriage_returns() {
        let plain_mode = TermFlags::default();
        // No brackets available, so this is the dangerous case the shell will execute —
        // but the line endings must still be what a shell reads as a line.
        assert_eq!(encode_paste("one\ntwo", plain_mode), b"one\rtwo");
        // CRLF collapses to one CR rather than producing a blank line after each.
        assert_eq!(encode_paste("one\r\ntwo", plain_mode), b"one\rtwo");
    }

    #[test]
    fn bracketed_paste_wraps_the_text_so_the_shell_does_not_run_it() {
        let bracketed = TermFlags { bracketed_paste: true, ..TermFlags::default() };
        assert_eq!(
            encode_paste("rm -rf /\nsudo reboot", bracketed),
            b"\x1b[200~rm -rf /\rsudo reboot\x1b[201~"
        );
    }

    #[test]
    fn a_paste_cannot_close_its_own_bracket() {
        let bracketed = TermFlags { bracketed_paste: true, ..TermFlags::default() };
        // Clipboard content carrying the end marker would otherwise leave the rest
        // outside the brackets, where the shell runs it. Losing the ESC is enough: what
        // remains is inert text the shell reads as literal characters, which is exactly
        // what the brackets promise.
        let bytes = encode_paste("safe\x1b[201~rm -rf /", bracketed);
        assert_eq!(bytes, b"\x1b[200~safe[201~rm -rf /\x1b[201~");
    }

    #[test]
    fn a_split_end_marker_cannot_be_spliced_back_together() {
        let bracketed = TermFlags { bracketed_paste: true, ..TermFlags::default() };
        // Stripping the literal marker in one pass leaves `ESC [ 20` and `1~` adjacent,
        // which rejoin into a real end marker on the wire. Dropping ESC has no such seam.
        let bytes = encode_paste("\x1b[20\x1b[201~1~rm -rf /", bracketed);

        let body = &bytes[6..bytes.len() - 6];
        assert!(
            !body.windows(6).any(|w| w == b"\x1b[201~"),
            "no end marker may survive inside the brackets: {:?}",
            String::from_utf8_lossy(body)
        );
        // Exactly one bracket pair, at the ends where this function put them.
        assert_eq!(bytes.iter().filter(|&&b| b == 0x1b).count(), 2);
    }

    #[test]
    fn an_unbracketed_paste_also_drops_escapes() {
        // Without DEC 2004 there is no bracket to break, but clipboard content is still
        // free to drive the terminal with a raw escape the user never looked at.
        let bytes = encode_paste("safe\x1b[2Jclear", TermFlags::default());
        assert_eq!(bytes, b"safe[2Jclear");
    }

    #[test]
    fn pasting_multibyte_text_keeps_its_bytes() {
        let bracketed = TermFlags { bracketed_paste: true, ..TermFlags::default() };
        let bytes = encode_paste("a\u{e7}\u{e3}o", bracketed);
        // Byte-for-byte, not char-for-char: the brackets are ASCII around UTF-8.
        assert!(bytes.windows(6).any(|w| w == "a\u{e7}\u{e3}o".as_bytes()));
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[200~a\u{e7}\u{e3}o\x1b[201~");
    }
}
