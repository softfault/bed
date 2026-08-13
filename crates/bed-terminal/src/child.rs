//! xterm-compatible encoding for input sent to child terminal sessions.

use crate::{Key, Modifiers, SpecialKey};
use bed_vt100::TerminalModes;

pub fn encode_child_key(key: &Key, modes: TerminalModes) -> Vec<u8> {
    match key {
        Key::Char(character) => character.to_string().into_bytes(),
        Key::Paste(text) if modes.bracketed_paste => {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        }
        Key::Paste(text) => text.as_bytes().to_vec(),
        Key::Tab => vec![b'\t'],
        Key::BackTab => b"\x1b[Z".to_vec(),
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::ArrowUp => navigation(b'A', modes.application_cursor),
        Key::ArrowDown => navigation(b'B', modes.application_cursor),
        Key::ArrowRight => navigation(b'C', modes.application_cursor),
        Key::ArrowLeft => navigation(b'D', modes.application_cursor),
        Key::Home => navigation(b'H', modes.application_cursor),
        Key::End => navigation(b'F', modes.application_cursor),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Ctrl(character) => encode_control(*character).into_iter().collect(),
        Key::Modified(key, modifiers) => modified_key(*key, *modifiers),
        Key::Escape => vec![0x1b],
        Key::Resize | Key::Unknown => Vec::new(),
    }
}

fn navigation(final_byte: u8, application_cursor: bool) -> Vec<u8> {
    let prefix: &[u8] = if application_cursor {
        b"\x1bO"
    } else {
        b"\x1b["
    };
    let mut bytes = prefix.to_vec();
    bytes.push(final_byte);
    bytes
}

fn modified_key(key: SpecialKey, modifiers: Modifiers) -> Vec<u8> {
    let modifier = 1
        + usize::from(modifiers.shift)
        + 2 * usize::from(modifiers.alt)
        + 4 * usize::from(modifiers.control);
    match key {
        SpecialKey::ArrowUp => format!("\x1b[1;{modifier}A").into_bytes(),
        SpecialKey::ArrowDown => format!("\x1b[1;{modifier}B").into_bytes(),
        SpecialKey::ArrowRight => format!("\x1b[1;{modifier}C").into_bytes(),
        SpecialKey::ArrowLeft => format!("\x1b[1;{modifier}D").into_bytes(),
        SpecialKey::Home => format!("\x1b[1;{modifier}H").into_bytes(),
        SpecialKey::End => format!("\x1b[1;{modifier}F").into_bytes(),
        SpecialKey::Delete => format!("\x1b[3;{modifier}~").into_bytes(),
        SpecialKey::PageUp => format!("\x1b[5;{modifier}~").into_bytes(),
        SpecialKey::PageDown => format!("\x1b[6;{modifier}~").into_bytes(),
    }
}

fn encode_control(character: char) -> Option<u8> {
    match character.to_ascii_lowercase() {
        'a'..='z' => Some(character.to_ascii_lowercase() as u8 - b'a' + 1),
        '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::encode_child_key;
    use crate::{Key, Modifiers, SpecialKey};
    use bed_vt100::TerminalModes;

    #[test]
    fn encodes_text_controls_and_editing_keys() {
        let modes = TerminalModes::default();
        assert_eq!(encode_child_key(&Key::Char('好'), modes), "好".as_bytes());
        assert_eq!(encode_child_key(&Key::Ctrl('c'), modes), b"\x03");
        assert_eq!(encode_child_key(&Key::Ctrl('\\'), modes), b"\x1c");
        assert_eq!(encode_child_key(&Key::Ctrl('['), modes), b"\x1b");
        assert_eq!(encode_child_key(&Key::BackTab, modes), b"\x1b[Z");
        assert_eq!(encode_child_key(&Key::Backspace, modes), b"\x7f");
        assert!(encode_child_key(&Key::Unknown, modes).is_empty());
    }

    #[test]
    fn honors_application_cursor_mode() {
        let normal = TerminalModes::default();
        let application = TerminalModes {
            application_cursor: true,
            ..TerminalModes::default()
        };
        assert_eq!(encode_child_key(&Key::ArrowUp, normal), b"\x1b[A");
        assert_eq!(encode_child_key(&Key::ArrowUp, application), b"\x1bOA");
        assert_eq!(encode_child_key(&Key::Home, application), b"\x1bOH");
    }

    #[test]
    fn encodes_xterm_navigation_modifiers() {
        assert_eq!(
            encode_child_key(
                &Key::Modified(
                    SpecialKey::ArrowLeft,
                    Modifiers {
                        shift: true,
                        control: true,
                        ..Modifiers::default()
                    }
                ),
                TerminalModes::default()
            ),
            b"\x1b[1;6D"
        );
        assert_eq!(
            encode_child_key(
                &Key::Modified(
                    SpecialKey::Delete,
                    Modifiers {
                        alt: true,
                        ..Modifiers::default()
                    }
                ),
                TerminalModes::default()
            ),
            b"\x1b[3;3~"
        );
    }

    #[test]
    fn wraps_paste_only_when_requested_by_the_child() {
        let key = Key::Paste("one\ntwo".to_owned());
        assert_eq!(
            encode_child_key(&key, TerminalModes::default()),
            b"one\ntwo"
        );
        assert_eq!(
            encode_child_key(
                &key,
                TerminalModes {
                    bracketed_paste: true,
                    ..TerminalModes::default()
                }
            ),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
    }
}
