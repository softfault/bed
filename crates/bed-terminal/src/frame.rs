use crate::TerminalSize;
use bed_vt100::{Attributes, Color};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ESCAPE: u8 = 0x1b;
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const RESET_STYLE: &[u8] = b"\x1b[m";
const BLOCK_CURSOR: &[u8] = b"\x1b[2 q";
const BAR_CURSOR: &[u8] = b"\x1b[6 q";

#[derive(Default)]
pub(super) struct FrameRenderer {
    previous: Option<Frame>,
}

impl FrameRenderer {
    pub(super) fn reset(&mut self) {
        self.previous = None;
    }

    pub(super) fn render(&mut self, bytes: &[u8], size: TerminalSize) -> Vec<u8> {
        let Some(current) = Frame::parse(bytes, size) else {
            self.previous = None;
            return bytes.to_vec();
        };
        let output = self.previous.as_ref().map_or_else(
            || bytes.to_vec(),
            |previous| {
                if previous.size() == current.size() {
                    diff_frames(previous, &current)
                } else {
                    bytes.to_vec()
                }
            },
        );
        self.previous = Some(current);
        output
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    rows: Vec<Vec<FrameCell>>,
    cursor: FrameCursor,
    attributes: Attributes,
    cursor_shape: CursorShape,
}

impl Frame {
    fn new(size: TerminalSize) -> Self {
        let rows = size.rows.max(1);
        let columns = size.columns.max(1);
        Self {
            rows: vec![vec![FrameCell::default(); columns]; rows],
            cursor: FrameCursor {
                visible: true,
                ..FrameCursor::default()
            },
            attributes: Attributes::default(),
            cursor_shape: CursorShape::Block,
        }
    }

    fn parse(bytes: &[u8], size: TerminalSize) -> Option<Self> {
        let mut frame = Self::new(size);
        frame.apply(bytes).then_some(frame)
    }

    fn size(&self) -> (usize, usize) {
        (self.rows.len(), self.rows[0].len())
    }

    fn apply(&mut self, bytes: &[u8]) -> bool {
        let mut offset = 0;
        while offset < bytes.len() {
            if bytes[offset] == ESCAPE {
                let Some(consumed) = self.apply_escape(&bytes[offset..]) else {
                    return false;
                };
                offset += consumed;
                continue;
            }
            let end = bytes[offset..]
                .iter()
                .position(|&byte| byte == ESCAPE)
                .map_or(bytes.len(), |length| offset + length);
            let Ok(text) = std::str::from_utf8(&bytes[offset..end]) else {
                return false;
            };
            self.write_text(text);
            offset = end;
        }
        true
    }

    fn apply_escape(&mut self, bytes: &[u8]) -> Option<usize> {
        if bytes.get(1) != Some(&b'[') {
            return None;
        }
        let final_offset = bytes[2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))?
            + 2;
        let body = &bytes[2..final_offset];
        match bytes[final_offset] {
            b'H' | b'f' => self.move_cursor(body)?,
            b'J' if body == b"2" => self.clear(),
            b'm' => self.set_graphics(body)?,
            b'h' if body == b"?25" => self.cursor.visible = true,
            b'l' if body == b"?25" => self.cursor.visible = false,
            b'q' if body == b"2 " => self.cursor_shape = CursorShape::Block,
            b'q' if body == b"6 " => self.cursor_shape = CursorShape::Bar,
            _ => return None,
        }
        Some(final_offset + 1)
    }

    fn move_cursor(&mut self, body: &[u8]) -> Option<()> {
        let mut parameters = body.split(|&byte| byte == b';');
        let row = parse_parameter(parameters.next().unwrap_or_default(), 1)?;
        let column = parse_parameter(parameters.next().unwrap_or_default(), 1)?;
        if parameters.next().is_some() {
            return None;
        }
        self.cursor.row = row.saturating_sub(1).min(self.rows.len() - 1);
        self.cursor.column = column
            .saturating_sub(1)
            .min(self.rows[0].len().saturating_sub(1));
        Some(())
    }

    fn clear(&mut self) {
        let (rows, columns) = self.size();
        self.rows = vec![vec![FrameCell::default(); columns]; rows];
    }

    fn set_graphics(&mut self, body: &[u8]) -> Option<()> {
        let parameters = if body.is_empty() {
            vec![0]
        } else {
            body.split(|&byte| byte == b';')
                .map(|parameter| parse_parameter(parameter, 0))
                .collect::<Option<Vec<_>>>()?
        };
        let mut index = 0;
        while index < parameters.len() {
            match parameters[index] {
                0 => self.attributes = Attributes::default(),
                1 => self.attributes.bold = true,
                2 => self.attributes.dim = true,
                3 => self.attributes.italic = true,
                4 => self.attributes.underline = true,
                5 => self.attributes.blink = true,
                7 => self.attributes.inverse = true,
                8 => self.attributes.hidden = true,
                9 => self.attributes.strikethrough = true,
                22 => {
                    self.attributes.bold = false;
                    self.attributes.dim = false;
                }
                23 => self.attributes.italic = false,
                24 => self.attributes.underline = false,
                25 => self.attributes.blink = false,
                27 => self.attributes.inverse = false,
                28 => self.attributes.hidden = false,
                29 => self.attributes.strikethrough = false,
                30..=37 => {
                    self.attributes.foreground = Color::Indexed((parameters[index] - 30) as u8)
                }
                39 => self.attributes.foreground = Color::Default,
                40..=47 => {
                    self.attributes.background = Color::Indexed((parameters[index] - 40) as u8)
                }
                49 => self.attributes.background = Color::Default,
                90..=97 => {
                    self.attributes.foreground = Color::Indexed((parameters[index] - 90 + 8) as u8)
                }
                100..=107 => {
                    self.attributes.background = Color::Indexed((parameters[index] - 100 + 8) as u8)
                }
                38 | 48 => {
                    let foreground = parameters[index] == 38;
                    let (color, consumed) = parse_extended_color(&parameters, index)?;
                    index += consumed;
                    if foreground {
                        self.attributes.foreground = color;
                    } else {
                        self.attributes.background = color;
                    }
                }
                _ => return None,
            }
            index += 1;
        }
        Some(())
    }

    fn write_text(&mut self, text: &str) {
        for grapheme in text.graphemes(true) {
            self.write_grapheme(grapheme);
        }
    }

    fn write_grapheme(&mut self, grapheme: &str) {
        let columns = self.rows[0].len();
        let width = UnicodeWidthStr::width(grapheme);
        if width == 0 {
            self.append_zero_width(grapheme);
            return;
        }
        let width = width.min(2);
        if self.cursor.column >= columns || self.cursor.column + width > columns {
            return;
        }
        let row = self.cursor.row;
        let column = self.cursor.column;
        for target in column..column + width {
            self.clear_occupied_cell(row, target);
        }
        self.rows[row][column] = FrameCell {
            contents: CellContents::from_grapheme(grapheme),
            attributes: self.attributes,
            continuation: false,
        };
        if width == 2 {
            self.rows[row][column + 1] = FrameCell {
                contents: CellContents::Blank,
                attributes: self.attributes,
                continuation: true,
            };
        }
        self.cursor.column = self.cursor.column.saturating_add(width);
    }

    fn append_zero_width(&mut self, grapheme: &str) {
        if self.cursor.column == 0 {
            return;
        }
        let row = &mut self.rows[self.cursor.row];
        let mut column = self.cursor.column.min(row.len()).saturating_sub(1);
        if row[column].continuation {
            let Some(leading) = column.checked_sub(1) else {
                return;
            };
            column = leading;
        }
        row[column].contents.push_str(grapheme);
    }

    fn clear_occupied_cell(&mut self, row: usize, column: usize) {
        if self.rows[row][column].continuation {
            if let Some(leading) = column.checked_sub(1) {
                self.rows[row][leading] = FrameCell::default();
            }
        } else if self.rows[row]
            .get(column + 1)
            .is_some_and(|cell| cell.continuation)
        {
            self.rows[row][column + 1] = FrameCell::default();
        }
        self.rows[row][column] = FrameCell::default();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameCell {
    contents: CellContents,
    attributes: Attributes,
    continuation: bool,
}

impl Default for FrameCell {
    fn default() -> Self {
        Self {
            contents: CellContents::Blank,
            attributes: Attributes::default(),
            continuation: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CellContents {
    Blank,
    Character(char),
    Grapheme(String),
}

impl CellContents {
    fn from_grapheme(grapheme: &str) -> Self {
        if grapheme == " " {
            return Self::Blank;
        }
        let mut characters = grapheme.chars();
        let first = characters.next().expect("graphemes are non-empty");
        if characters.next().is_none() {
            Self::Character(first)
        } else {
            Self::Grapheme(grapheme.to_owned())
        }
    }

    fn push_str(&mut self, suffix: &str) {
        match self {
            Self::Blank => *self = Self::Grapheme(format!(" {suffix}")),
            Self::Character(character) => {
                *self = Self::Grapheme(format!("{character}{suffix}"));
            }
            Self::Grapheme(contents) => contents.push_str(suffix),
        }
    }

    fn write_to(&self, output: &mut Vec<u8>) {
        match self {
            Self::Blank => output.push(b' '),
            Self::Character(character) => {
                let mut buffer = [0; 4];
                output.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
            Self::Grapheme(contents) => output.extend_from_slice(contents.as_bytes()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FrameCursor {
    row: usize,
    column: usize,
    visible: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CursorShape {
    #[default]
    Block,
    Bar,
}

fn parse_extended_color(parameters: &[usize], index: usize) -> Option<(Color, usize)> {
    match *parameters.get(index + 1)? {
        5 => {
            let value = u8::try_from(*parameters.get(index + 2)?).ok()?;
            Some((Color::Indexed(value), 2))
        }
        2 => {
            let red = u8::try_from(*parameters.get(index + 2)?).ok()?;
            let green = u8::try_from(*parameters.get(index + 3)?).ok()?;
            let blue = u8::try_from(*parameters.get(index + 4)?).ok()?;
            Some((Color::Rgb(red, green, blue), 4))
        }
        _ => None,
    }
}

fn parse_parameter(parameter: &[u8], default: usize) -> Option<usize> {
    if parameter.is_empty() {
        return Some(default);
    }
    std::str::from_utf8(parameter).ok()?.parse().ok()
}

fn diff_frames(previous: &Frame, current: &Frame) -> Vec<u8> {
    let mut spans = Vec::new();
    for (row, (before, after)) in previous.rows.iter().zip(&current.rows).enumerate() {
        let mut changed: Vec<_> = before
            .iter()
            .zip(after)
            .map(|(before, after)| before != after)
            .collect();
        for column in 0..changed.len() {
            if !changed[column] {
                continue;
            }
            if (before[column].continuation || after[column].continuation) && column > 0 {
                changed[column - 1] = true;
            }
            if column + 1 < changed.len()
                && (before[column + 1].continuation || after[column + 1].continuation)
            {
                changed[column + 1] = true;
            }
        }
        let mut column = 0;
        while column < changed.len() {
            if !changed[column] {
                column += 1;
                continue;
            }
            let start = column;
            while column < changed.len() && changed[column] {
                column += 1;
            }
            spans.push((row, start..column));
        }
    }

    let cursor_changed = previous.cursor != current.cursor;
    let shape_changed = previous.cursor_shape != current.cursor_shape;
    if spans.is_empty() && !cursor_changed && !shape_changed {
        return Vec::new();
    }

    let mut output = Vec::new();
    if !spans.is_empty() {
        output.extend_from_slice(HIDE_CURSOR);
    }
    for (row, columns) in spans {
        move_to(&mut output, row, columns.start);
        output.extend_from_slice(RESET_STYLE);
        let mut attributes = Attributes::default();
        emit_cells(&mut output, &current.rows[row], columns, &mut attributes);
    }
    if !output.is_empty() {
        output.extend_from_slice(RESET_STYLE);
    }
    move_to(&mut output, current.cursor.row, current.cursor.column);
    output.extend_from_slice(match current.cursor_shape {
        CursorShape::Block => BLOCK_CURSOR,
        CursorShape::Bar => BAR_CURSOR,
    });
    output.extend_from_slice(if current.cursor.visible {
        SHOW_CURSOR
    } else {
        HIDE_CURSOR
    });
    output
}

fn emit_cells(
    output: &mut Vec<u8>,
    row: &[FrameCell],
    columns: Range<usize>,
    attributes: &mut Attributes,
) {
    for cell in &row[columns] {
        if cell.continuation {
            continue;
        }
        if cell.attributes != *attributes {
            output.extend_from_slice(RESET_STYLE);
            output.extend_from_slice(sgr_attributes(cell.attributes).as_bytes());
            *attributes = cell.attributes;
        }
        cell.contents.write_to(output);
    }
}

fn move_to(output: &mut Vec<u8>, row: usize, column: usize) {
    output.extend_from_slice(format!("\x1b[{};{}H", row + 1, column + 1).as_bytes());
}

fn sgr_attributes(attributes: Attributes) -> String {
    let mut parameters = Vec::new();
    if attributes.bold {
        parameters.push("1".to_owned());
    }
    if attributes.dim {
        parameters.push("2".to_owned());
    }
    if attributes.italic {
        parameters.push("3".to_owned());
    }
    if attributes.underline {
        parameters.push("4".to_owned());
    }
    if attributes.blink {
        parameters.push("5".to_owned());
    }
    if attributes.inverse {
        parameters.push("7".to_owned());
    }
    if attributes.hidden {
        parameters.push("8".to_owned());
    }
    if attributes.strikethrough {
        parameters.push("9".to_owned());
    }
    push_sgr_color(&mut parameters, attributes.foreground, true);
    push_sgr_color(&mut parameters, attributes.background, false);
    if parameters.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", parameters.join(";"))
    }
}

fn push_sgr_color(parameters: &mut Vec<String>, color: Color, foreground: bool) {
    let base = if foreground { 30 } else { 40 };
    match color {
        Color::Default => {}
        Color::Indexed(index @ 0..=7) => parameters.push((base + u16::from(index)).to_string()),
        Color::Indexed(index @ 8..=15) => {
            parameters.push((base + 60 + u16::from(index - 8)).to_string());
        }
        Color::Indexed(index) => {
            parameters.push(format!("{};5;{index}", if foreground { 38 } else { 48 }));
        }
        Color::Rgb(red, green, blue) => parameters.push(format!(
            "{};2;{red};{green};{blue}",
            if foreground { 38 } else { 48 }
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, FrameRenderer};
    use crate::TerminalSize;

    const SIZE: TerminalSize = TerminalSize {
        rows: 3,
        columns: 16,
    };

    #[test]
    fn keeps_the_first_frame_and_diffs_later_frames() {
        let first = frame("alpha", 1, 1, false);
        let second = frame("alphi", 1, 1, false);
        let mut renderer = FrameRenderer::default();

        assert_eq!(renderer.render(&first, SIZE), first);
        let update = renderer.render(&second, SIZE);

        assert!(!contains(&update, b"\x1b[2J"));
        assert!(contains(&update, b"\x1b[1;5H"));
        assert!(!contains(&update, b"alpha"));
        assert_applies(&first, &update, &second, SIZE);
        assert!(renderer.render(&second, SIZE).is_empty());
    }

    #[test]
    fn tracks_zwj_graphemes_at_their_display_columns() {
        let first = frame("a👩🏽‍💻b", 1, 1, false);
        let second = frame("a👩🏽‍💻c", 1, 1, false);
        let mut renderer = FrameRenderer::default();
        renderer.render(&first, SIZE);

        let update = renderer.render(&second, SIZE);

        assert!(contains(&update, b"\x1b[1;4H"));
        assert!(contains(&update, b"c"));
        assert!(!contains(&update, "👩🏽‍💻".as_bytes()));
        assert_applies(&first, &update, &second, SIZE);
    }

    #[test]
    fn replaces_complete_wide_cells_and_preserves_attributes() {
        let first = frame("\x1b[7m好\x1b[mX", 1, 1, false);
        let second = frame("  \x1b[38;2;1;2;3mY\x1b[m", 1, 1, false);
        let mut renderer = FrameRenderer::default();
        renderer.render(&first, SIZE);

        let update = renderer.render(&second, SIZE);

        assert!(contains(&update, b"\x1b[1;1H"));
        assert!(contains(&update, b"\x1b[38;2;1;2;3mY"));
        assert_applies(&first, &update, &second, SIZE);
    }

    #[test]
    fn emits_cursor_only_updates_without_redrawing_cells() {
        let first = frame("same", 1, 1, false);
        let second = frame("same", 2, 4, true);
        let mut renderer = FrameRenderer::default();
        renderer.render(&first, SIZE);

        let update = renderer.render(&second, SIZE);

        assert_eq!(update, b"\x1b[2;4H\x1b[6 q\x1b[?25h");
        assert_applies(&first, &update, &second, SIZE);
    }

    #[test]
    fn redraws_the_complete_frame_after_a_resize() {
        let first = frame("small", 1, 1, false);
        let resized = frame("large", 1, 1, false);
        let resized_size = TerminalSize {
            rows: 4,
            columns: 20,
        };
        let mut renderer = FrameRenderer::default();
        renderer.render(&first, SIZE);

        assert_eq!(renderer.render(&resized, resized_size), resized);
    }

    fn frame(text: &str, cursor_row: usize, cursor_column: usize, bar: bool) -> Vec<u8> {
        format!(
            "\x1b[?25l\x1b[H\x1b[2J\x1b[1;1H{text}\x1b[{cursor_row};{cursor_column}H{}\x1b[?25h",
            if bar { "\x1b[6 q" } else { "\x1b[2 q" }
        )
        .into_bytes()
    }

    fn assert_applies(before: &[u8], update: &[u8], after: &[u8], size: TerminalSize) {
        let mut applied = Frame::parse(before, size).unwrap();
        assert!(applied.apply(update));
        assert_eq!(applied, Frame::parse(after, size).unwrap());
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
