//! UTF-8 and virtual-terminal input decoder for byte-stream backends.
//!
//! ECMA-48 defines CSI framing and final-byte ranges. Terminal key encodings
//! are de facto xterm protocols, including the modifier parameter used here.
//! This module converts complete input into semantic [`Key`] values so encoded
//! bytes never cross the terminal boundary.
//!
//! Authoritative references:
//! - ECMA-48, [Control Functions for Coded Character Sets](https://ecma-international.org/publications-and-standards/standards/ecma-48/)
//! - xterm [`ctlseqs`](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html),
//!   especially “PC-Style Function Keys”

use super::{Key, Modifiers, SpecialKey};
use anyhow::{Context, Result};
use std::io::Read;

// A fixed parser bound prevents an unterminated or hostile sequence from
// growing memory. Supported navigation sequences are much shorter than this.
const MAX_ESCAPE_SEQUENCE: usize = 16;
// Paste input is delivered as one editor event. Bound it independently from
// navigation sequences, while still consuming an oversized paste terminator.
const MAX_PASTE_BYTES: usize = 16 * 1024 * 1024;
const BRACKETED_PASTE_START: &[u8] = b"200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

pub(super) struct VtInput<R> {
    reader: R,
    pending_byte: Option<u8>,
}

impl<R: Read> VtInput<R> {
    pub(super) fn new(reader: R) -> Self {
        Self {
            reader,
            pending_byte: None,
        }
    }

    pub(super) fn read_key(&mut self) -> Result<Option<Key>> {
        let Some(byte) = self.read_byte()? else {
            return Ok(None);
        };

        let key = match byte {
            b'\r' | b'\n' => Key::Enter,
            b'\t' => Key::Tab,
            8 | 127 => Key::Backspace,
            1..=26 => Key::Ctrl(char::from(byte + b'a' - 1)),
            0x1c..=0x1f => Key::Ctrl(char::from(byte + b'@')),
            b'\x1b' => self.read_escape_sequence()?,
            32..=126 => Key::Char(char::from(byte)),
            128..=255 => self.read_utf8_character(byte)?,
            _ => Key::Unknown,
        };
        Ok(Some(key))
    }

    fn read_escape_sequence(&mut self) -> Result<Key> {
        let Some(first) = self.read_byte()? else {
            // On POSIX backends this is the VTIME inter-byte timeout, which
            // distinguishes a standalone Escape from a following sequence.
            return Ok(Key::Escape);
        };
        match first {
            b'[' | b'O' => self.read_csi_sequence(),
            byte => {
                self.pending_byte = Some(byte);
                Ok(Key::Escape)
            }
        }
    }

    fn read_utf8_character(&mut self, first: u8) -> Result<Key> {
        let length = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Ok(Key::Unknown),
        };
        let mut bytes = [0; 4];
        bytes[0] = first;
        for byte in &mut bytes[1..length] {
            let Some(next) = self.read_byte()? else {
                return Ok(Key::Unknown);
            };
            *byte = next;
        }
        Ok(std::str::from_utf8(&bytes[..length])
            .ok()
            .and_then(|text| text.chars().next())
            .map_or(Key::Unknown, Key::Char))
    }

    fn read_csi_sequence(&mut self) -> Result<Key> {
        let mut sequence = [0; MAX_ESCAPE_SEQUENCE];
        let mut length = 0;
        let mut overflowed = false;
        loop {
            let Some(byte) = self.read_byte()? else {
                return Ok(Key::Unknown);
            };
            if length < sequence.len() {
                sequence[length] = byte;
                length += 1;
            } else {
                overflowed = true;
            }
            // ECMA-48 assigns 0x40..=0x7e as CSI final bytes.
            if (0x40..=0x7e).contains(&byte) {
                if overflowed {
                    return Ok(Key::Unknown);
                }
                if &sequence[..length] == BRACKETED_PASTE_START {
                    return self.read_bracketed_paste();
                }
                return Ok(parse_csi(&sequence[..length]));
            }
        }
    }

    fn read_bracketed_paste(&mut self) -> Result<Key> {
        let mut bytes = Vec::new();
        let mut matched = 0;
        let mut overflowed = false;
        let mut empty_reads = 0;

        loop {
            let Some(byte) = self.read_byte()? else {
                empty_reads += 1;
                if empty_reads >= 10 {
                    return Ok(Key::Unknown);
                }
                continue;
            };
            empty_reads = 0;

            if byte == BRACKETED_PASTE_END[matched] {
                matched += 1;
                if matched == BRACKETED_PASTE_END.len() {
                    break;
                }
                continue;
            }

            if matched > 0 {
                append_paste_bytes(&mut bytes, &BRACKETED_PASTE_END[..matched], &mut overflowed);
                matched = 0;
                if byte == BRACKETED_PASTE_END[0] {
                    matched = 1;
                    continue;
                }
            }
            append_paste_bytes(&mut bytes, &[byte], &mut overflowed);
        }

        if overflowed {
            return Ok(Key::Unknown);
        }
        Ok(String::from_utf8(bytes).map_or(Key::Unknown, Key::Paste))
    }

    fn read_byte(&mut self) -> Result<Option<u8>> {
        if self.pending_byte.is_some() {
            return Ok(self.pending_byte.take());
        }
        let mut byte = [0];
        match self
            .reader
            .read(&mut byte)
            .context("failed to read terminal input")?
        {
            0 => Ok(None),
            _ => Ok(Some(byte[0])),
        }
    }
}

fn append_paste_bytes(bytes: &mut Vec<u8>, incoming: &[u8], overflowed: &mut bool) {
    if bytes.len().saturating_add(incoming.len()) > MAX_PASTE_BYTES {
        *overflowed = true;
    } else if !*overflowed {
        bytes.extend_from_slice(incoming);
    }
}

fn parse_csi(sequence: &[u8]) -> Key {
    let Some((&final_byte, parameters)) = sequence.split_last() else {
        return Key::Unknown;
    };
    let Some(parameters) = parse_parameters(parameters) else {
        return Key::Unknown;
    };

    match final_byte {
        // Terminals conventionally report Shift-Tab as CSI Z (CBT). Normalize
        // that encoding to the same semantic event as the Windows backend.
        b'Z' if parameters.is_empty() => Key::BackTab,
        b'A' => navigation_key(SpecialKey::ArrowUp, modifier_parameter(&parameters)),
        b'B' => navigation_key(SpecialKey::ArrowDown, modifier_parameter(&parameters)),
        b'C' => navigation_key(SpecialKey::ArrowRight, modifier_parameter(&parameters)),
        b'D' => navigation_key(SpecialKey::ArrowLeft, modifier_parameter(&parameters)),
        b'H' => navigation_key(SpecialKey::Home, modifier_parameter(&parameters)),
        b'F' => navigation_key(SpecialKey::End, modifier_parameter(&parameters)),
        // xterm's VT220-style editing keys use a numeric key identifier and
        // '~' final byte. 1/7 and 4/8 are the common Home/End variants.
        b'~' => match parameters.first().copied() {
            Some(3) => navigation_key(SpecialKey::Delete, modifier_parameter(&parameters)),
            Some(1 | 7) => navigation_key(SpecialKey::Home, modifier_parameter(&parameters)),
            Some(4 | 8) => navigation_key(SpecialKey::End, modifier_parameter(&parameters)),
            Some(5) => navigation_key(SpecialKey::PageUp, modifier_parameter(&parameters)),
            Some(6) => navigation_key(SpecialKey::PageDown, modifier_parameter(&parameters)),
            _ => Key::Unknown,
        },
        _ => Key::Unknown,
    }
}

fn parse_parameters(bytes: &[u8]) -> Option<Vec<usize>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_digit() || *byte == b';')
    {
        return None;
    }
    bytes
        .split(|byte| *byte == b';')
        .map(|parameter| {
            if parameter.is_empty() {
                Some(1)
            } else {
                std::str::from_utf8(parameter).ok()?.parse().ok()
            }
        })
        .collect()
}

fn modifier_parameter(parameters: &[usize]) -> usize {
    // xterm puts the modifier in the second parameter and defaults it to 1.
    parameters.get(1).copied().unwrap_or(1)
}

fn navigation_key(key: SpecialKey, modifier: usize) -> Key {
    let Some(modifiers) = decode_modifiers(modifier) else {
        return Key::Unknown;
    };
    if modifiers == Modifiers::default() {
        match key {
            SpecialKey::Delete => Key::Delete,
            SpecialKey::ArrowUp => Key::ArrowUp,
            SpecialKey::ArrowDown => Key::ArrowDown,
            SpecialKey::ArrowLeft => Key::ArrowLeft,
            SpecialKey::ArrowRight => Key::ArrowRight,
            SpecialKey::Home => Key::Home,
            SpecialKey::End => Key::End,
            SpecialKey::PageUp => Key::PageUp,
            SpecialKey::PageDown => Key::PageDown,
        }
    } else {
        Key::Modified(key, modifiers)
    }
}

fn decode_modifiers(parameter: usize) -> Option<Modifiers> {
    // xterm encodes modifiers as 1 + Shift + 2*Alt + 4*Control.
    let bits = parameter.checked_sub(1)?;
    if bits > 0b111 {
        return None;
    }
    Some(Modifiers {
        shift: bits & 0b001 != 0,
        alt: bits & 0b010 != 0,
        control: bits & 0b100 != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::{Key, Modifiers, SpecialKey, VtInput, parse_csi};
    use std::io::Cursor;

    #[test]
    fn decodes_ascii_and_utf8_input() {
        let mut input = VtInput::new(Cursor::new("a好\t\x1b[Z\n".as_bytes()));

        assert_eq!(input.read_key().unwrap(), Some(Key::Char('a')));
        assert_eq!(input.read_key().unwrap(), Some(Key::Char('好')));
        assert_eq!(input.read_key().unwrap(), Some(Key::Tab));
        assert_eq!(input.read_key().unwrap(), Some(Key::BackTab));
        assert_eq!(input.read_key().unwrap(), Some(Key::Enter));
        assert_eq!(input.read_key().unwrap(), None);
    }

    #[test]
    fn decodes_terminal_mode_control_prefixes() {
        let mut input = VtInput::new(Cursor::new(b"\x1c\x1d\x1e\x1f"));

        assert_eq!(input.read_key().unwrap(), Some(Key::Ctrl('\\')));
        assert_eq!(input.read_key().unwrap(), Some(Key::Ctrl(']')));
        assert_eq!(input.read_key().unwrap(), Some(Key::Ctrl('^')));
        assert_eq!(input.read_key().unwrap(), Some(Key::Ctrl('_')));
    }

    #[test]
    fn decodes_bracketed_paste_as_one_event() {
        let bytes = b"\x1b[200~one\n\xe4\xbd\xa0\xe5\xa5\xbd\t\x1b[x\x1b[201~";
        let mut input = VtInput::new(Cursor::new(bytes));

        assert_eq!(
            input.read_key().unwrap(),
            Some(Key::Paste("one\n你好\t\x1b[x".to_owned()))
        );
        assert_eq!(input.read_key().unwrap(), None);
    }

    #[test]
    fn parses_plain_navigation_sequences() {
        assert_eq!(parse_csi(b"Z"), Key::BackTab);
        assert_eq!(parse_csi(b"A"), Key::ArrowUp);
        assert_eq!(parse_csi(b"3~"), Key::Delete);
        assert_eq!(parse_csi(b"F"), Key::End);
        assert_eq!(parse_csi(b"5~"), Key::PageUp);
        assert_eq!(parse_csi(b"6~"), Key::PageDown);
    }

    #[test]
    fn parses_xterm_modifier_parameters() {
        assert_eq!(
            parse_csi(b"1;5D"),
            Key::Modified(
                SpecialKey::ArrowLeft,
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                }
            )
        );
        assert_eq!(
            parse_csi(b"3;2~"),
            Key::Modified(
                SpecialKey::Delete,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                }
            )
        );
        assert_eq!(
            parse_csi(b"1;3C"),
            Key::Modified(
                SpecialKey::ArrowRight,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                }
            )
        );
    }

    #[test]
    fn rejects_unsupported_complete_sequences() {
        assert_eq!(parse_csi(b"1;2Z"), Key::Unknown);
        assert_eq!(parse_csi(b"1;9D"), Key::Unknown);
        assert_eq!(parse_csi(b"?25h"), Key::Unknown);
    }
}
