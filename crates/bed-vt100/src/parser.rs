use crate::screen::{Attributes, Color, Screen, TerminalModes};

const ESC: u8 = 0x1b;
const MAX_OSC_BYTES: usize = 4096;
const REPLACEMENT_CHARACTER: char = '\u{fffd}';

#[derive(Clone, Debug, Default)]
enum ParserState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi(Vec<u8>),
    Osc {
        bytes: Vec<u8>,
        escape: bool,
        overflowed: bool,
    },
    IgnoreString {
        escape: bool,
    },
}

#[derive(Clone, Debug)]
pub struct TerminalEmulator {
    primary: Screen,
    alternate: Screen,
    alternate_active: bool,
    modes: TerminalModes,
    state: ParserState,
    utf8: Vec<u8>,
    utf8_expected: usize,
    title: String,
    responses: Vec<u8>,
    unsupported_sequences: usize,
    bell_count: usize,
    visual_bell_count: usize,
    reset_count: u64,
    scrollback_capacity: usize,
}

impl TerminalEmulator {
    pub fn new(rows: usize, columns: usize, scrollback_capacity: usize) -> Self {
        Self {
            primary: Screen::new(rows, columns, scrollback_capacity, true),
            alternate: Screen::new(rows, columns, 0, false),
            alternate_active: false,
            modes: TerminalModes::default(),
            state: ParserState::default(),
            utf8: Vec::with_capacity(4),
            utf8_expected: 0,
            title: String::new(),
            responses: Vec::new(),
            unsupported_sequences: 0,
            bell_count: 0,
            visual_bell_count: 0,
            reset_count: 0,
            scrollback_capacity,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.process_byte(byte);
        }
    }

    /// Finishes an output stream after PTY or ConPTY EOF.
    ///
    /// An incomplete UTF-8 scalar becomes a replacement character. Partial
    /// control strings are discarded so a later, unrelated stream cannot
    /// complete them accidentally.
    pub fn finish(&mut self) {
        self.flush_incomplete_utf8();
        if !matches!(self.state, ParserState::Ground) {
            self.unsupported_sequences = self.unsupported_sequences.saturating_add(1);
        }
        self.state = ParserState::Ground;
    }

    pub fn screen(&self) -> &Screen {
        if self.alternate_active {
            &self.alternate
        } else {
            &self.primary
        }
    }

    pub fn primary_screen(&self) -> &Screen {
        &self.primary
    }

    pub fn alternate_screen(&self) -> &Screen {
        &self.alternate
    }

    pub fn modes(&self) -> TerminalModes {
        self.modes
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn alternate_screen_active(&self) -> bool {
        self.alternate_active
    }

    pub fn unsupported_sequence_count(&self) -> usize {
        self.unsupported_sequences
    }

    pub fn bell_count(&self) -> usize {
        self.bell_count
    }

    pub fn visual_bell_count(&self) -> usize {
        self.visual_bell_count
    }

    pub fn reset_count(&self) -> u64 {
        self.reset_count
    }

    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    pub fn set_size(&mut self, rows: usize, columns: usize) {
        self.primary.resize(rows, columns);
        self.alternate.resize(rows, columns);
    }

    fn screen_mut(&mut self) -> &mut Screen {
        if self.alternate_active {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    fn process_byte(&mut self, byte: u8) {
        let state = std::mem::take(&mut self.state);
        match state {
            ParserState::Ground => self.process_ground(byte),
            ParserState::Escape => self.process_escape(byte),
            ParserState::EscapeIntermediate => {
                if byte == ESC {
                    self.state = ParserState::Escape;
                } else {
                    self.state = ParserState::Ground;
                }
            }
            ParserState::Csi(mut bytes) => {
                if byte == ESC {
                    self.unsupported_sequences += 1;
                    self.state = ParserState::Escape;
                } else if matches!(byte, 0x18 | 0x1a) {
                    self.state = ParserState::Ground;
                } else if (0x40..=0x7e).contains(&byte) {
                    self.dispatch_csi(&bytes, byte);
                    self.state = ParserState::Ground;
                } else if (0x20..=0x3f).contains(&byte) && bytes.len() < 256 {
                    bytes.push(byte);
                    self.state = ParserState::Csi(bytes);
                } else if byte < 0x20 {
                    self.execute_control(byte);
                    self.state = ParserState::Csi(bytes);
                } else {
                    self.unsupported_sequences += 1;
                    self.state = ParserState::Ground;
                }
            }
            ParserState::Osc {
                mut bytes,
                mut escape,
                mut overflowed,
            } => {
                if matches!(byte, 0x18 | 0x1a) {
                    self.state = ParserState::Ground;
                } else if escape {
                    if byte == b'\\' {
                        if overflowed {
                            self.unsupported_sequences =
                                self.unsupported_sequences.saturating_add(1);
                        } else {
                            self.dispatch_osc(&bytes);
                        }
                        self.state = ParserState::Ground;
                    } else {
                        if bytes.len().saturating_add(2) <= MAX_OSC_BYTES {
                            bytes.push(ESC);
                            bytes.push(byte);
                        } else {
                            overflowed = true;
                        }
                        escape = false;
                        self.state = ParserState::Osc {
                            bytes,
                            escape,
                            overflowed,
                        };
                    }
                } else if byte == 0x07 {
                    if overflowed {
                        self.unsupported_sequences = self.unsupported_sequences.saturating_add(1);
                    } else {
                        self.dispatch_osc(&bytes);
                    }
                    self.state = ParserState::Ground;
                } else if byte == ESC {
                    escape = true;
                    self.state = ParserState::Osc {
                        bytes,
                        escape,
                        overflowed,
                    };
                } else {
                    if bytes.len() < MAX_OSC_BYTES {
                        bytes.push(byte);
                    } else {
                        overflowed = true;
                    }
                    self.state = ParserState::Osc {
                        bytes,
                        escape,
                        overflowed,
                    };
                }
            }
            ParserState::IgnoreString { mut escape } => {
                if matches!(byte, 0x18 | 0x1a) || (escape && byte == b'\\') {
                    self.state = ParserState::Ground;
                } else {
                    escape = byte == ESC;
                    self.state = ParserState::IgnoreString { escape };
                }
            }
        }
    }

    fn process_ground(&mut self, byte: u8) {
        if byte == ESC {
            self.flush_incomplete_utf8();
            self.state = ParserState::Escape;
        } else if byte < 0x20 || byte == 0x7f {
            self.flush_incomplete_utf8();
            self.execute_control(byte);
            self.state = ParserState::Ground;
        } else if byte < 0x80 {
            self.flush_incomplete_utf8();
            self.print_character(char::from(byte));
            self.state = ParserState::Ground;
        } else {
            self.process_utf8_byte(byte);
        }
    }

    fn process_escape(&mut self, byte: u8) {
        match byte {
            b'[' => self.state = ParserState::Csi(Vec::new()),
            b']' => {
                self.state = ParserState::Osc {
                    bytes: Vec::new(),
                    escape: false,
                    overflowed: false,
                };
            }
            b'P' | b'^' | b'_' => {
                self.unsupported_sequences += 1;
                self.state = ParserState::IgnoreString { escape: false };
            }
            b'7' => {
                self.screen_mut().save_cursor();
                self.state = ParserState::Ground;
            }
            b'8' => {
                self.screen_mut().restore_cursor();
                self.state = ParserState::Ground;
            }
            b'D' => {
                self.screen_mut().line_feed();
                self.state = ParserState::Ground;
            }
            b'E' => {
                self.screen_mut().carriage_return();
                self.screen_mut().line_feed();
                self.state = ParserState::Ground;
            }
            b'M' => {
                self.screen_mut().reverse_index();
                self.state = ParserState::Ground;
            }
            b'H' => {
                self.screen_mut().set_tab_stop();
                self.state = ParserState::Ground;
            }
            b'=' => {
                self.modes.application_keypad = true;
                self.state = ParserState::Ground;
            }
            b'>' => {
                self.modes.application_keypad = false;
                self.state = ParserState::Ground;
            }
            b'c' => {
                self.reset();
                self.state = ParserState::Ground;
            }
            b'g' => {
                self.visual_bell_count = self.visual_bell_count.saturating_add(1);
                self.state = ParserState::Ground;
            }
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' => {
                self.state = ParserState::EscapeIntermediate;
            }
            ESC => self.state = ParserState::Escape,
            _ => {
                self.unsupported_sequences += 1;
                self.state = ParserState::Ground;
            }
        }
    }

    fn execute_control(&mut self, byte: u8) {
        match byte {
            0x00 | 0x0e | 0x0f | 0x7f => {}
            0x07 => self.bell_count = self.bell_count.saturating_add(1),
            0x08 => self.screen_mut().backspace(),
            0x09 => self.screen_mut().tab(),
            0x0a..=0x0c => self.screen_mut().line_feed(),
            0x0d => self.screen_mut().carriage_return(),
            _ => {}
        }
    }

    fn process_utf8_byte(&mut self, byte: u8) {
        if self.utf8.is_empty() {
            self.utf8_expected = match byte {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => {
                    self.print_character(REPLACEMENT_CHARACTER);
                    return;
                }
            };
            self.utf8.push(byte);
            return;
        }

        if !(0x80..=0xbf).contains(&byte) {
            self.print_character(REPLACEMENT_CHARACTER);
            self.utf8.clear();
            self.utf8_expected = 0;
            self.process_ground(byte);
            return;
        }

        self.utf8.push(byte);
        if self.utf8.len() == self.utf8_expected {
            let character = std::str::from_utf8(&self.utf8)
                .ok()
                .and_then(|text| text.chars().next())
                .unwrap_or(REPLACEMENT_CHARACTER);
            self.utf8.clear();
            self.utf8_expected = 0;
            self.print_character(character);
        }
    }

    fn flush_incomplete_utf8(&mut self) {
        if !self.utf8.is_empty() {
            self.utf8.clear();
            self.utf8_expected = 0;
            self.print_character(REPLACEMENT_CHARACTER);
        }
    }

    fn print_character(&mut self, character: char) {
        let modes = self.modes;
        self.screen_mut().put_char(character, modes);
    }

    fn dispatch_csi(&mut self, bytes: &[u8], final_byte: u8) {
        let private = bytes
            .first()
            .copied()
            .filter(|byte| matches!(byte, b'?' | b'>'));
        let parameter_bytes = if private.is_some() {
            &bytes[1..]
        } else {
            bytes
        };
        if parameter_bytes
            .iter()
            .any(|byte| !byte.is_ascii_digit() && *byte != b';' && *byte != b':')
        {
            self.unsupported_sequences += 1;
            return;
        }
        let parameters = parse_parameters(parameter_bytes);
        let first = parameter(&parameters, 0, 1);
        match (private, final_byte) {
            (None, b'A') => {
                let origin = self.modes.origin;
                self.screen_mut()
                    .move_relative(-(first as isize), 0, origin);
            }
            (None, b'B') => {
                let origin = self.modes.origin;
                self.screen_mut().move_relative(first as isize, 0, origin);
            }
            (None, b'C') | (None, b'a') => {
                self.screen_mut().move_relative(0, first as isize, false)
            }
            (None, b'D') => self.screen_mut().move_relative(0, -(first as isize), false),
            (None, b'E') => {
                let origin = self.modes.origin;
                self.screen_mut().move_relative(first as isize, 0, origin);
                self.screen_mut().set_column(0);
            }
            (None, b'F') => {
                let origin = self.modes.origin;
                self.screen_mut()
                    .move_relative(-(first as isize), 0, origin);
                self.screen_mut().set_column(0);
            }
            (None, b'G') | (None, b'`') => self.screen_mut().set_column(first - 1),
            (None, b'H') | (None, b'f') => {
                let row = parameter(&parameters, 0, 1) - 1;
                let column = parameter(&parameters, 1, 1) - 1;
                let origin = self.modes.origin;
                self.screen_mut().set_position(row, column, origin);
            }
            (None, b'd') => {
                let origin = self.modes.origin;
                self.screen_mut().set_row(first - 1, origin);
            }
            (None, b'J') => self
                .screen_mut()
                .erase_display(raw_parameter(&parameters, 0, 0)),
            (None, b'K') => self
                .screen_mut()
                .erase_line(raw_parameter(&parameters, 0, 0)),
            (Some(b'?'), b'J') => self
                .screen_mut()
                .erase_display(raw_parameter(&parameters, 0, 0)),
            (Some(b'?'), b'K') => self
                .screen_mut()
                .erase_line(raw_parameter(&parameters, 0, 0)),
            (None, b'@') => self.screen_mut().insert_cells(first),
            (None, b'P') => self.screen_mut().delete_cells(first),
            (None, b'X') => self.screen_mut().erase_cells(first),
            (None, b'L') => self.screen_mut().insert_lines(first),
            (None, b'M') => self.screen_mut().delete_lines(first),
            (None, b'S') => self.screen_mut().scroll_up(first),
            (None, b'T') => self.screen_mut().scroll_down(first),
            (None, b'm') => self.apply_sgr(&parameters),
            (None, b'r') => {
                let height = self.screen().size().0;
                let top = parameter(&parameters, 0, 1) - 1;
                let bottom = parameter(&parameters, 1, height) - 1;
                let origin = self.modes.origin;
                self.screen_mut().set_margins(top, bottom, origin);
            }
            (None, b's') => self.screen_mut().save_cursor(),
            (None, b'u') => self.screen_mut().restore_cursor(),
            (None, b'g') => self.screen_mut().clear_tab_stop(first == 3),
            (None, b'h') => self.set_ansi_modes(&parameters, true),
            (None, b'l') => self.set_ansi_modes(&parameters, false),
            (Some(b'?'), b'h') => self.set_private_modes(&parameters, true),
            (Some(b'?'), b'l') => self.set_private_modes(&parameters, false),
            (None, b'n') => self.device_status(raw_parameter(&parameters, 0, 0), false),
            (Some(b'?'), b'n') => self.device_status(raw_parameter(&parameters, 0, 0), true),
            (None, b'c') => self.responses.extend_from_slice(b"\x1b[?1;2c"),
            (Some(b'>'), b'c') => self.responses.extend_from_slice(b"\x1b[>0;1;0c"),
            _ => self.unsupported_sequences += 1,
        }
    }

    fn apply_sgr(&mut self, parameters: &[Option<u16>]) {
        let mut attributes = self.screen().attributes();
        let parameters = if parameters.is_empty() {
            vec![Some(0)]
        } else {
            parameters.to_vec()
        };
        let mut index = 0;
        while index < parameters.len() {
            let value = parameters[index].unwrap_or(0);
            match value {
                0 => attributes = Attributes::default(),
                1 => attributes.bold = true,
                2 => attributes.dim = true,
                3 => attributes.italic = true,
                4 => attributes.underline = true,
                5 | 6 => attributes.blink = true,
                7 => attributes.inverse = true,
                8 => attributes.hidden = true,
                9 => attributes.strikethrough = true,
                22 => {
                    attributes.bold = false;
                    attributes.dim = false;
                }
                23 => attributes.italic = false,
                24 => attributes.underline = false,
                25 => attributes.blink = false,
                27 => attributes.inverse = false,
                28 => attributes.hidden = false,
                29 => attributes.strikethrough = false,
                30..=37 => attributes.foreground = Color::Indexed((value - 30) as u8),
                38 => {
                    index +=
                        apply_extended_color(&parameters[index + 1..], &mut attributes.foreground);
                }
                39 => attributes.foreground = Color::Default,
                40..=47 => attributes.background = Color::Indexed((value - 40) as u8),
                48 => {
                    index +=
                        apply_extended_color(&parameters[index + 1..], &mut attributes.background);
                }
                49 => attributes.background = Color::Default,
                90..=97 => attributes.foreground = Color::Indexed((value - 90 + 8) as u8),
                100..=107 => attributes.background = Color::Indexed((value - 100 + 8) as u8),
                _ => {}
            }
            index += 1;
        }
        self.screen_mut().set_attributes(attributes);
    }

    fn set_ansi_modes(&mut self, parameters: &[Option<u16>], enabled: bool) {
        for mode in parameters.iter().flatten() {
            match mode {
                4 => self.modes.insert = enabled,
                _ => self.unsupported_sequences += 1,
            }
        }
    }

    fn set_private_modes(&mut self, parameters: &[Option<u16>], enabled: bool) {
        for mode in parameters.iter().flatten() {
            match mode {
                1 => self.modes.application_cursor = enabled,
                6 => {
                    self.modes.origin = enabled;
                    self.screen_mut().set_position(0, 0, enabled);
                }
                7 => self.modes.automatic_wrap = enabled,
                25 => {
                    self.primary.set_cursor_visible(enabled);
                    self.alternate.set_cursor_visible(enabled);
                }
                47 | 1047 => self.use_alternate_screen(enabled, false),
                1048 => {
                    if enabled {
                        self.screen_mut().save_cursor();
                    } else {
                        self.screen_mut().restore_cursor();
                    }
                }
                1049 => self.use_alternate_screen(enabled, true),
                1000 | 1002 | 1003 => {
                    if enabled {
                        self.modes.mouse_tracking = Some(*mode);
                    } else if self.modes.mouse_tracking == Some(*mode) {
                        self.modes.mouse_tracking = None;
                    }
                }
                1006 => self.modes.sgr_mouse = enabled,
                2004 => self.modes.bracketed_paste = enabled,
                _ => self.unsupported_sequences += 1,
            }
        }
    }

    fn use_alternate_screen(&mut self, enabled: bool, save_cursor: bool) {
        if enabled == self.alternate_active {
            return;
        }
        if enabled {
            if save_cursor {
                self.primary.save_cursor();
            }
            self.alternate.clear();
            self.alternate_active = true;
        } else {
            self.alternate_active = false;
            if save_cursor {
                self.primary.restore_cursor();
            }
        }
    }

    fn device_status(&mut self, status: u16, private: bool) {
        match (private, status) {
            (false, 5) => self.responses.extend_from_slice(b"\x1b[0n"),
            (false, 6) => {
                let cursor = self.screen().cursor();
                self.responses.extend_from_slice(
                    format!("\x1b[{};{}R", cursor.row + 1, cursor.column + 1).as_bytes(),
                );
            }
            (true, 6) => {
                let cursor = self.screen().cursor();
                self.responses.extend_from_slice(
                    format!("\x1b[?{};{}R", cursor.row + 1, cursor.column + 1).as_bytes(),
                );
            }
            _ => self.unsupported_sequences += 1,
        }
    }

    fn dispatch_osc(&mut self, bytes: &[u8]) {
        let Some(separator) = bytes.iter().position(|byte| *byte == b';') else {
            self.unsupported_sequences += 1;
            return;
        };
        let command = std::str::from_utf8(&bytes[..separator])
            .ok()
            .and_then(|value| value.parse::<u16>().ok());
        match command {
            Some(0 | 2) => {
                self.title = String::from_utf8_lossy(&bytes[separator + 1..]).into_owned();
            }
            _ => self.unsupported_sequences += 1,
        }
    }

    fn reset(&mut self) {
        let (rows, columns) = self.screen().size();
        self.primary = Screen::new(rows, columns, self.scrollback_capacity, true);
        self.alternate = Screen::new(rows, columns, 0, false);
        self.alternate_active = false;
        self.modes = TerminalModes::default();
        self.utf8.clear();
        self.utf8_expected = 0;
        self.title.clear();
        self.responses.clear();
        self.reset_count = self.reset_count.saturating_add(1);
    }
}

fn parse_parameters(bytes: &[u8]) -> Vec<Option<u16>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|byte| *byte == b';' || *byte == b':')
        .map(|parameter| {
            if parameter.is_empty() {
                None
            } else {
                std::str::from_utf8(parameter).ok()?.parse().ok()
            }
        })
        .collect()
}

fn parameter(parameters: &[Option<u16>], index: usize, default: usize) -> usize {
    usize::from(
        parameters
            .get(index)
            .copied()
            .flatten()
            .unwrap_or(default as u16)
            .max(1),
    )
}

fn raw_parameter(parameters: &[Option<u16>], index: usize, default: u16) -> u16 {
    parameters.get(index).copied().flatten().unwrap_or(default)
}

fn apply_extended_color(parameters: &[Option<u16>], color: &mut Color) -> usize {
    match parameters.first().copied().flatten() {
        Some(5) if parameters.len() >= 2 => {
            if let Some(index) = parameters[1] {
                *color = Color::Indexed(index.min(255) as u8);
            }
            2
        }
        Some(2) if parameters.len() >= 4 => {
            if let (Some(red), Some(green), Some(blue)) =
                (parameters[1], parameters[2], parameters[3])
            {
                *color = Color::Rgb(
                    red.min(255) as u8,
                    green.min(255) as u8,
                    blue.min(255) as u8,
                );
            }
            4
        }
        Some(_) => 1,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_OSC_BYTES, TerminalEmulator};
    use crate::{Color, Screen, TerminalModes};

    #[test]
    fn parses_text_controls_cursor_movement_and_erasing() {
        let mut terminal = TerminalEmulator::new(3, 10, 10);
        terminal.process(b"one\r\ntwo\x1b[1A\x1b[5C!\x1b[K");

        assert_eq!(terminal.screen().row(0).unwrap().text(), "one     !");
        assert_eq!(terminal.screen().row(1).unwrap().text(), "two");
        assert_eq!(terminal.screen().cursor().row, 0);
        assert_eq!(terminal.screen().cursor().column, 9);
    }

    #[test]
    fn keeps_parser_state_across_every_input_split() {
        let fixture = "start\r\n好👩🏽‍💻\x1b[31;48;2;1;2;3mred\x1b[0m\x1b]2;bed title\x1b\\";
        let mut expected = TerminalEmulator::new(5, 20, 20);
        expected.process(fixture.as_bytes());

        for split in 0..=fixture.len() {
            let mut actual = TerminalEmulator::new(5, 20, 20);
            actual.process(&fixture.as_bytes()[..split]);
            actual.process(&fixture.as_bytes()[split..]);
            assert_eq!(actual.screen().contents(), expected.screen().contents());
            assert_eq!(actual.screen().cursor(), expected.screen().cursor());
            assert_eq!(actual.title(), expected.title());
        }
    }

    #[test]
    fn stores_graphemes_and_wide_continuation_cells() {
        let mut terminal = TerminalEmulator::new(2, 8, 0);
        terminal.process("a👩🏽‍💻好".as_bytes());

        assert_eq!(terminal.screen().cell(0, 0).unwrap().contents(), "a");
        assert_eq!(terminal.screen().cell(0, 1).unwrap().contents(), "👩🏽‍💻");
        assert!(terminal.screen().cell(0, 2).unwrap().is_continuation());
        assert_eq!(terminal.screen().cell(0, 3).unwrap().contents(), "好");
        assert!(terminal.screen().cell(0, 4).unwrap().is_continuation());
        assert_eq!(terminal.screen().cursor().column, 5);
    }

    #[test]
    fn bounds_primary_scrollback_and_keeps_alternate_output_separate() {
        let mut terminal = TerminalEmulator::new(2, 8, 2);
        terminal.process(b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(terminal.primary_screen().scrollback().len(), 2);
        assert_eq!(terminal.primary_screen().history_rows_pushed(), 2);
        assert_eq!(terminal.primary_screen().history_rows_discarded(), 0);
        assert_eq!(terminal.primary_screen().scrollback()[0].text(), "one");

        terminal.process(b"\x1b[?1049halt\r\nscreen\r\nmore");
        assert!(terminal.alternate_screen_active());
        assert!(terminal.alternate_screen().scrollback().is_empty());
        terminal.process(b"\x1b[?1049l");

        assert!(!terminal.alternate_screen_active());
        assert_eq!(terminal.primary_screen().scrollback().len(), 2);
        assert_eq!(terminal.primary_screen().history_rows_pushed(), 2);
        assert!(terminal.primary_screen().contents().contains("four"));

        terminal.process(b"\r\nfive");
        assert_eq!(terminal.primary_screen().scrollback().len(), 2);
        assert_eq!(terminal.primary_screen().history_rows_pushed(), 3);
        assert_eq!(terminal.primary_screen().history_rows_discarded(), 1);
    }

    #[test]
    fn tracks_modes_titles_colors_and_device_responses() {
        let mut terminal = TerminalEmulator::new(4, 12, 0);
        terminal.process(
            b"\x1b[?1h\x1b[?25l\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[38;5;200;48;2;1;2;3mX\x1b]0;shell\x07\x1b[6n",
        );

        assert_eq!(
            terminal.modes(),
            TerminalModes {
                application_cursor: true,
                bracketed_paste: true,
                mouse_tracking: Some(1002),
                sgr_mouse: true,
                ..TerminalModes::default()
            }
        );
        assert!(!terminal.screen().cursor().visible);
        assert_eq!(terminal.title(), "shell");
        let attributes = terminal.screen().cell(0, 0).unwrap().attributes();
        assert_eq!(attributes.foreground, Color::Indexed(200));
        assert_eq!(attributes.background, Color::Rgb(1, 2, 3));
        assert_eq!(terminal.take_responses(), b"\x1b[1;2R");
    }

    #[test]
    fn supports_visual_bells_and_private_erase_aliases() {
        let mut terminal = TerminalEmulator::new(2, 8, 2);
        terminal.process(b"\x1b[Hbefore\x1b[2;1Hsecond\x1b[H\x1b[?2K");
        assert_eq!(terminal.screen().row(0).unwrap().text(), "");
        assert_eq!(terminal.screen().row(1).unwrap().text(), "second");

        terminal.process(b"\x1b[2;1H\x1b[?2K\x1b[Hafter\x1b[?2J\x1bg");

        assert_eq!(terminal.visual_bell_count(), 1);
        assert_eq!(terminal.screen().row(0).unwrap().text(), "");
        assert_eq!(terminal.screen().row(1).unwrap().text(), "");
        assert_eq!(terminal.unsupported_sequence_count(), 0);
    }

    #[test]
    fn accepts_bel_and_st_terminated_titles() {
        let mut terminal = TerminalEmulator::new(2, 20, 0);
        terminal.process(b"\x1b]0;first\x07");
        assert_eq!(terminal.title(), "first");

        terminal.process(b"\x1b]2;second\x1b\\");
        assert_eq!(terminal.title(), "second");
        assert_eq!(terminal.bell_count(), 0);
    }

    #[test]
    fn rejects_oversized_osc_and_recovers_after_its_terminator() {
        let mut terminal = TerminalEmulator::new(2, 20, 0);
        terminal.process(b"\x1b]2;kept\x07");

        let mut oversized = b"\x1b]2;".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_OSC_BYTES));
        oversized.extend_from_slice(b"\x1b\\OK");
        terminal.process(&oversized);

        assert_eq!(terminal.title(), "kept");
        assert!(terminal.screen().contents().contains("OK"));
        assert_eq!(terminal.unsupported_sequence_count(), 1);

        terminal.process(b"\x1b]2;recovered\x07");
        assert_eq!(terminal.title(), "recovered");
    }

    #[test]
    fn resize_clamps_state_and_repairs_wide_cells() {
        let mut terminal = TerminalEmulator::new(3, 5, 10);
        terminal.process("123好".as_bytes());
        terminal.set_size(2, 4);

        assert_eq!(terminal.screen().size(), (2, 4));
        assert!(terminal.screen().cursor().row < 2);
        assert!(terminal.screen().cursor().column < 4);
        assert!(!terminal.screen().cell(0, 3).unwrap().is_continuation());
    }

    #[test]
    fn malformed_and_unsupported_input_is_recoverable() {
        let mut terminal = TerminalEmulator::new(2, 20, 0);
        terminal
            .process(b"a\xf0\x28\x8c\x28b\xe2\x1b[2COK\x1b[999\x18C\x1bPignored\x1adone\x1b%G!");

        let contents = terminal.screen().contents();
        assert!(contents.contains("a�(�(b"));
        assert!(contents.contains("OKCdone!"));
        assert_eq!(terminal.unsupported_sequence_count(), 1);
    }

    #[test]
    fn finishing_a_stream_resolves_partial_input() {
        let mut terminal = TerminalEmulator::new(2, 20, 0);
        terminal.process(b"text\xe2\x82");
        terminal.finish();
        assert_eq!(terminal.screen().row(0).unwrap().text(), "text�");

        terminal.process(b"ok\x1b[31");
        terminal.finish();
        terminal.process(b"mplain");
        assert!(terminal.screen().contents().contains("okmplain"));
        assert_eq!(terminal.unsupported_sequence_count(), 1);
    }

    #[test]
    fn erase_scrollback_preserves_the_visible_screen() {
        let mut terminal = TerminalEmulator::new(2, 8, 10);
        terminal.process(b"one\r\ntwo\r\nthree");
        assert!(!terminal.screen().scrollback().is_empty());
        let visible = terminal.screen().contents();

        terminal.process(b"\x1b[3J");

        assert!(terminal.screen().scrollback().is_empty());
        assert_eq!(terminal.primary_screen().history_rows_discarded(), 1);
        assert_eq!(terminal.screen().contents(), visible);
    }

    #[test]
    fn hard_reset_advances_the_screen_generation() {
        let mut terminal = TerminalEmulator::new(2, 8, 10);
        terminal.process(b"one\r\ntwo\r\nthree");
        assert_eq!(terminal.reset_count(), 0);

        terminal.process(b"\x1bc");

        assert_eq!(terminal.reset_count(), 1);
        assert!(terminal.primary_screen().scrollback().is_empty());
        assert_eq!(terminal.screen().contents(), "\n");
    }

    #[test]
    fn private_modes_apply_across_screens_and_respect_origin_margins() {
        let mut terminal = TerminalEmulator::new(6, 12, 0);
        terminal.process(b"\x1b[?25l\x1b[?1049h");
        assert!(!terminal.screen().cursor().visible);

        terminal.process(b"\x1b[2;5r\x1b[?6h\x1b[99B");
        assert_eq!(terminal.screen().cursor().row, 4);
        terminal.process(b"\x1b[99A");
        assert_eq!(terminal.screen().cursor().row, 1);

        terminal.process(b"\x1b[?1049l");
        assert!(!terminal.screen().cursor().visible);
    }

    #[test]
    fn supports_default_and_custom_tab_stops() {
        let mut terminal = TerminalEmulator::new(2, 20, 0);
        terminal.process(b"a\tb");
        assert_eq!(terminal.screen().cell(0, 8).unwrap().contents(), "b");

        terminal.process(b"\r\x1b[3g\x1b[5G\x1bH\rX\tY");
        assert_eq!(terminal.screen().cell(0, 4).unwrap().contents(), "Y");
    }

    #[test]
    fn mixed_operations_preserve_grid_invariants() {
        let mut terminal = TerminalEmulator::new(4, 9, 3);
        for bytes in [
            "one好\r\ntwo👩🏽‍💻\r\nthree\r\nfour".as_bytes(),
            b"\x1b[2;2H\x1b[2@AB\x1b[3P\x1b[2L\x1b[1M",
            b"\x1b[2;4r\x1b[2S\x1b[1T\x1b[2J",
            "\x1b[?1049h备用屏好\x1b[?1049l".as_bytes(),
        ] {
            terminal.process(bytes);
            assert_grid_invariants(terminal.primary_screen());
            assert_grid_invariants(terminal.alternate_screen());
        }
        for (rows, columns) in [(0, 0), (8, 3), (2, 12), (1, 1)] {
            terminal.set_size(rows, columns);
            assert_grid_invariants(terminal.primary_screen());
            assert_grid_invariants(terminal.alternate_screen());
        }
    }

    fn assert_grid_invariants(screen: &Screen) {
        let (rows, columns) = screen.size();
        assert!(rows > 0);
        assert!(columns > 0);
        assert_eq!(screen.rows().len(), rows);
        assert!(screen.cursor().row < rows);
        assert!(screen.cursor().column < columns);
        for row in screen.rows() {
            assert_eq!(row.cells().len(), columns);
            for (column, cell) in row.cells().iter().enumerate() {
                if cell.is_continuation() {
                    assert!(column > 0);
                    assert!(!row.cells()[column - 1].is_continuation());
                    assert_eq!(
                        unicode_width::UnicodeWidthStr::width(row.cells()[column - 1].contents()),
                        2
                    );
                }
            }
        }
    }
}
