use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    contents: String,
    attributes: Attributes,
    continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            contents: String::from(" "),
            attributes: Attributes::default(),
            continuation: false,
        }
    }
}

impl Cell {
    pub fn contents(&self) -> &str {
        &self.contents
    }

    pub fn attributes(&self) -> Attributes {
        self.attributes
    }

    pub fn is_continuation(&self) -> bool {
        self.continuation
    }

    fn blank(attributes: Attributes) -> Self {
        Self {
            attributes,
            ..Self::default()
        }
    }

    fn grapheme(contents: String, attributes: Attributes) -> Self {
        Self {
            contents,
            attributes,
            continuation: false,
        }
    }

    fn continuation(attributes: Attributes) -> Self {
        Self {
            contents: String::new(),
            attributes,
            continuation: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    cells: Vec<Cell>,
    wrapped: bool,
}

impl Row {
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    pub fn text(&self) -> String {
        let mut text = String::new();
        for cell in &self.cells {
            if !cell.continuation {
                text.push_str(&cell.contents);
            }
        }
        text.trim_end_matches(' ').to_owned()
    }

    fn blank(columns: usize, attributes: Attributes) -> Self {
        Self {
            cells: vec![Cell::blank(attributes); columns],
            wrapped: false,
        }
    }

    fn repair_wide_cells(&mut self) {
        let columns = self.cells.len();
        for column in 0..columns {
            if self.cells[column].continuation {
                let valid = column > 0
                    && !self.cells[column - 1].continuation
                    && display_width(&self.cells[column - 1].contents) == 2;
                if !valid {
                    self.cells[column] = Cell::default();
                }
            } else if display_width(&self.cells[column].contents) == 2 {
                if column + 1 < columns {
                    let attributes = self.cells[column].attributes;
                    self.cells[column + 1] = Cell::continuation(attributes);
                } else {
                    self.cells[column] = Cell::default();
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    pub visible: bool,
    pub pending_wrap: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalModes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
    pub origin: bool,
    pub insert: bool,
    pub automatic_wrap: bool,
    pub mouse_tracking: Option<u16>,
    pub sgr_mouse: bool,
}

impl Default for TerminalModes {
    fn default() -> Self {
        Self {
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: false,
            origin: false,
            insert: false,
            automatic_wrap: true,
            mouse_tracking: None,
            sgr_mouse: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Screen {
    rows: Vec<Row>,
    scrollback: VecDeque<Row>,
    scrollback_capacity: usize,
    history_rows_pushed: u64,
    history_rows_discarded: u64,
    cursor: Cursor,
    saved_cursor: Cursor,
    attributes: Attributes,
    saved_attributes: Attributes,
    top_margin: usize,
    bottom_margin: usize,
    tab_stops: Vec<bool>,
    history_enabled: bool,
}

impl Screen {
    pub(crate) fn new(
        rows: usize,
        columns: usize,
        scrollback_capacity: usize,
        history_enabled: bool,
    ) -> Self {
        let rows = rows.max(1);
        let columns = columns.max(1);
        Self {
            rows: vec![Row::blank(columns, Attributes::default()); rows],
            scrollback: VecDeque::new(),
            scrollback_capacity,
            history_rows_pushed: 0,
            history_rows_discarded: 0,
            cursor: Cursor {
                visible: true,
                ..Cursor::default()
            },
            saved_cursor: Cursor {
                visible: true,
                ..Cursor::default()
            },
            attributes: Attributes::default(),
            saved_attributes: Attributes::default(),
            top_margin: 0,
            bottom_margin: rows - 1,
            tab_stops: default_tab_stops(columns),
            history_enabled,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.rows.len(), self.rows[0].cells.len())
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, row: usize) -> Option<&Row> {
        self.rows.get(row)
    }

    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        self.rows.get(row)?.cells.get(column)
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn scrollback(&self) -> &VecDeque<Row> {
        &self.scrollback
    }

    /// Returns the cumulative number of rows added to this screen's history.
    ///
    /// Unlike `scrollback().len()`, this advances after the bounded history is
    /// full, allowing a view to remain anchored while new output arrives.
    pub fn history_rows_pushed(&self) -> u64 {
        self.history_rows_pushed
    }

    /// Returns the cumulative number of rows permanently removed from the
    /// front of this screen's history.
    pub fn history_rows_discarded(&self) -> u64 {
        self.history_rows_discarded
    }

    pub fn contents(&self) -> String {
        self.rows
            .iter()
            .map(Row::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn attributes(&self) -> Attributes {
        self.attributes
    }

    pub(crate) fn set_attributes(&mut self, attributes: Attributes) {
        self.attributes = attributes;
    }

    pub(crate) fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor.visible = visible;
    }

    pub(crate) fn set_tab_stop(&mut self) {
        self.tab_stops[self.cursor.column] = true;
    }

    pub(crate) fn clear(&mut self) {
        let (rows, columns) = self.size();
        self.rows = vec![Row::blank(columns, Attributes::default()); rows];
        self.discard_scrollback();
        self.cursor = Cursor {
            visible: self.cursor.visible,
            ..Cursor::default()
        };
        self.saved_cursor = self.cursor;
        self.attributes = Attributes::default();
        self.saved_attributes = Attributes::default();
        self.top_margin = 0;
        self.bottom_margin = rows - 1;
    }

    pub(crate) fn resize(&mut self, rows: usize, columns: usize) {
        let rows = rows.max(1);
        let columns = columns.max(1);
        for row in &mut self.rows {
            row.cells
                .resize_with(columns, || Cell::blank(Attributes::default()));
            row.repair_wide_cells();
            row.wrapped = false;
        }
        while self.rows.len() > rows {
            let removed = self.rows.remove(0);
            if self.history_enabled {
                self.push_scrollback(removed);
            }
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        while self.rows.len() < rows {
            if self.history_enabled
                && let Some(row) = self.scrollback.pop_back()
            {
                self.rows.insert(0, resized_row(row, columns));
                self.cursor.row = self.cursor.row.saturating_add(1);
            } else {
                self.rows.push(Row::blank(columns, Attributes::default()));
            }
        }
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.column = self.cursor.column.min(columns - 1);
        self.cursor.pending_wrap = false;
        self.saved_cursor.row = self.saved_cursor.row.min(rows - 1);
        self.saved_cursor.column = self.saved_cursor.column.min(columns - 1);
        self.top_margin = 0;
        self.bottom_margin = rows - 1;
        self.tab_stops = default_tab_stops(columns);
    }

    pub(crate) fn put_char(&mut self, character: char, modes: TerminalModes) {
        if self.try_extend_previous_grapheme(character) {
            return;
        }
        let mut contents = character.to_string();
        let mut width = display_width(&contents);
        if width == 0 {
            contents.insert(0, ' ');
            width = 1;
        }
        width = width.min(2);

        if self.cursor.pending_wrap {
            if modes.automatic_wrap {
                self.rows[self.cursor.row].wrapped = true;
                self.carriage_return();
                self.line_feed();
            }
            self.cursor.pending_wrap = false;
        }
        let columns = self.size().1;
        if width == 2 && self.cursor.column + 1 >= columns {
            if modes.automatic_wrap {
                self.rows[self.cursor.row].wrapped = true;
                self.carriage_return();
                self.line_feed();
            } else {
                return;
            }
        }
        if modes.insert {
            self.insert_cells(width);
        }
        self.clear_wide_cell_at(self.cursor.row, self.cursor.column);
        let row = &mut self.rows[self.cursor.row];
        row.cells[self.cursor.column] = Cell::grapheme(contents, self.attributes);
        if width == 2 {
            row.cells[self.cursor.column + 1] = Cell::continuation(self.attributes);
        }
        if self.cursor.column + width >= columns {
            self.cursor.column = columns - 1;
            self.cursor.pending_wrap = modes.automatic_wrap;
        } else {
            self.cursor.column += width;
        }
    }

    fn try_extend_previous_grapheme(&mut self, character: char) -> bool {
        let Some((row, column)) = self.previous_leading_cell() else {
            return false;
        };
        let previous = self.rows[row].cells[column].contents.clone();
        if previous == " " {
            return false;
        }
        let mut combined = previous.clone();
        combined.push(character);
        if combined.graphemes(true).count() != 1 {
            return false;
        }
        let old_width = display_width(&previous).clamp(1, 2);
        let new_width = display_width(&combined).clamp(1, 2);
        let columns = self.size().1;
        if new_width == 2 && column + 1 >= columns {
            return false;
        }
        self.rows[row].cells[column].contents = combined;
        if old_width == 2 && new_width == 1 && column + 1 < columns {
            self.rows[row].cells[column + 1] = Cell::default();
            self.rewind_cursor_after_width_change(1);
        } else if old_width == 1 && new_width == 2 {
            let attributes = self.rows[row].cells[column].attributes;
            self.rows[row].cells[column + 1] = Cell::continuation(attributes);
            self.advance_cursor_after_width_change(1);
        }
        true
    }

    fn previous_leading_cell(&self) -> Option<(usize, usize)> {
        let (row, mut column) = if self.cursor.pending_wrap {
            (self.cursor.row, self.size().1 - 1)
        } else if self.cursor.column > 0 {
            (self.cursor.row, self.cursor.column - 1)
        } else {
            return None;
        };
        if self.rows[row].cells[column].continuation {
            column = column.checked_sub(1)?;
        }
        Some((row, column))
    }

    fn rewind_cursor_after_width_change(&mut self, amount: usize) {
        if self.cursor.pending_wrap {
            self.cursor.pending_wrap = false;
        } else {
            self.cursor.column = self.cursor.column.saturating_sub(amount);
        }
    }

    fn advance_cursor_after_width_change(&mut self, amount: usize) {
        let columns = self.size().1;
        if self.cursor.pending_wrap || self.cursor.column + amount >= columns {
            self.cursor.column = columns - 1;
            self.cursor.pending_wrap = true;
        } else {
            self.cursor.column += amount;
        }
    }

    pub(crate) fn backspace(&mut self) {
        self.cursor.column = self.cursor.column.saturating_sub(1);
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn tab(&mut self) {
        let columns = self.size().1;
        self.cursor.column = ((self.cursor.column + 1)..columns)
            .find(|column| self.tab_stops[*column])
            .unwrap_or(columns - 1);
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn carriage_return(&mut self) {
        self.cursor.column = 0;
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn line_feed(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.row == self.bottom_margin {
            self.scroll_up(1);
        } else {
            self.cursor.row = (self.cursor.row + 1).min(self.size().0 - 1);
        }
    }

    pub(crate) fn reverse_index(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.row == self.top_margin {
            self.scroll_down(1);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
    }

    pub(crate) fn move_relative(&mut self, rows: isize, columns: isize, origin: bool) {
        let (height, width) = self.size();
        let min_row = if origin { self.top_margin } else { 0 };
        let max_row = if origin {
            self.bottom_margin
        } else {
            height - 1
        };
        self.cursor.row = self
            .cursor
            .row
            .saturating_add_signed(rows)
            .clamp(min_row, max_row);
        self.cursor.column = self
            .cursor
            .column
            .saturating_add_signed(columns)
            .min(width - 1);
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn set_position(&mut self, row: usize, column: usize, origin: bool) {
        let (height, width) = self.size();
        let row = if origin {
            self.top_margin.saturating_add(row).min(self.bottom_margin)
        } else {
            row.min(height - 1)
        };
        self.cursor.row = row;
        self.cursor.column = column.min(width - 1);
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn set_column(&mut self, column: usize) {
        self.cursor.column = column.min(self.size().1 - 1);
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn set_row(&mut self, row: usize, origin: bool) {
        self.set_position(row, self.cursor.column, origin);
    }

    pub(crate) fn erase_display(&mut self, mode: u16) {
        let (height, width) = self.size();
        match mode {
            0 => {
                self.erase_row_range(self.cursor.row, self.cursor.column, width);
                for row in self.cursor.row + 1..height {
                    self.erase_row_range(row, 0, width);
                }
            }
            1 => {
                for row in 0..self.cursor.row {
                    self.erase_row_range(row, 0, width);
                }
                self.erase_row_range(self.cursor.row, 0, self.cursor.column + 1);
            }
            2 => {
                for row in 0..height {
                    self.erase_row_range(row, 0, width);
                }
            }
            3 => self.discard_scrollback(),
            _ => {}
        }
    }

    pub(crate) fn erase_line(&mut self, mode: u16) {
        let width = self.size().1;
        match mode {
            0 => self.erase_row_range(self.cursor.row, self.cursor.column, width),
            1 => self.erase_row_range(self.cursor.row, 0, self.cursor.column + 1),
            2 => self.erase_row_range(self.cursor.row, 0, width),
            _ => {}
        }
    }

    pub(crate) fn insert_cells(&mut self, count: usize) {
        let width = self.size().1;
        let count = count.min(width - self.cursor.column);
        let row = &mut self.rows[self.cursor.row];
        for _ in 0..count {
            row.cells
                .insert(self.cursor.column, Cell::blank(self.attributes));
            row.cells.pop();
        }
        row.repair_wide_cells();
        row.wrapped = false;
    }

    pub(crate) fn delete_cells(&mut self, count: usize) {
        let width = self.size().1;
        let count = count.min(width - self.cursor.column);
        let row = &mut self.rows[self.cursor.row];
        for _ in 0..count {
            row.cells.remove(self.cursor.column);
            row.cells.push(Cell::blank(self.attributes));
        }
        row.repair_wide_cells();
        row.wrapped = false;
    }

    pub(crate) fn erase_cells(&mut self, count: usize) {
        let end = (self.cursor.column + count).min(self.size().1);
        self.erase_row_range(self.cursor.row, self.cursor.column, end);
    }

    pub(crate) fn insert_lines(&mut self, count: usize) {
        if !(self.top_margin..=self.bottom_margin).contains(&self.cursor.row) {
            return;
        }
        let count = count.min(self.bottom_margin - self.cursor.row + 1);
        let columns = self.size().1;
        for _ in 0..count {
            self.rows
                .insert(self.cursor.row, Row::blank(columns, Attributes::default()));
            self.rows.remove(self.bottom_margin + 1);
        }
    }

    pub(crate) fn delete_lines(&mut self, count: usize) {
        if !(self.top_margin..=self.bottom_margin).contains(&self.cursor.row) {
            return;
        }
        let count = count.min(self.bottom_margin - self.cursor.row + 1);
        let columns = self.size().1;
        for _ in 0..count {
            self.rows.remove(self.cursor.row);
            self.rows.insert(
                self.bottom_margin,
                Row::blank(columns, Attributes::default()),
            );
        }
    }

    pub(crate) fn scroll_up(&mut self, count: usize) {
        let count = count.min(self.bottom_margin - self.top_margin + 1);
        let columns = self.size().1;
        let full_screen = self.top_margin == 0 && self.bottom_margin + 1 == self.size().0;
        for _ in 0..count {
            let removed = self.rows.remove(self.top_margin);
            if self.history_enabled && full_screen {
                self.push_scrollback(removed);
            }
            self.rows.insert(
                self.bottom_margin,
                Row::blank(columns, Attributes::default()),
            );
        }
    }

    pub(crate) fn scroll_down(&mut self, count: usize) {
        let count = count.min(self.bottom_margin - self.top_margin + 1);
        let columns = self.size().1;
        for _ in 0..count {
            self.rows.remove(self.bottom_margin);
            self.rows
                .insert(self.top_margin, Row::blank(columns, Attributes::default()));
        }
    }

    pub(crate) fn set_margins(&mut self, top: usize, bottom: usize, origin: bool) {
        let height = self.size().0;
        if top < bottom && bottom < height {
            self.top_margin = top;
            self.bottom_margin = bottom;
            self.set_position(0, 0, origin);
        }
    }

    pub(crate) fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
        self.saved_attributes = self.attributes;
    }

    pub(crate) fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.cursor.row = self.cursor.row.min(self.size().0 - 1);
        self.cursor.column = self.cursor.column.min(self.size().1 - 1);
        self.attributes = self.saved_attributes;
    }

    pub(crate) fn clear_tab_stop(&mut self, all: bool) {
        if all {
            self.tab_stops.fill(false);
        } else {
            self.tab_stops[self.cursor.column] = false;
        }
    }

    fn erase_row_range(&mut self, row: usize, start: usize, end: usize) {
        for column in start..end {
            self.clear_wide_cell_at(row, column);
            self.rows[row].cells[column] = Cell::blank(self.attributes);
        }
        if end == self.size().1 {
            self.rows[row].wrapped = false;
        }
    }

    fn clear_wide_cell_at(&mut self, row: usize, column: usize) {
        if self.rows[row].cells[column].continuation {
            if column > 0 {
                self.rows[row].cells[column - 1] = Cell::blank(self.attributes);
            }
        } else if display_width(&self.rows[row].cells[column].contents) == 2
            && column + 1 < self.size().1
        {
            self.rows[row].cells[column + 1] = Cell::blank(self.attributes);
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.scrollback_capacity == 0 {
            return;
        }
        self.history_rows_pushed = self.history_rows_pushed.saturating_add(1);
        self.scrollback.push_back(row);
        while self.scrollback.len() > self.scrollback_capacity {
            self.scrollback.pop_front();
            self.history_rows_discarded = self.history_rows_discarded.saturating_add(1);
        }
    }

    fn discard_scrollback(&mut self) {
        self.history_rows_discarded = self
            .history_rows_discarded
            .saturating_add(u64::try_from(self.scrollback.len()).unwrap_or(u64::MAX));
        self.scrollback.clear();
    }
}

fn resized_row(mut row: Row, columns: usize) -> Row {
    row.cells
        .resize_with(columns, || Cell::blank(Attributes::default()));
    row.repair_wide_cells();
    row.wrapped = false;
    row
}

fn default_tab_stops(columns: usize) -> Vec<bool> {
    (0..columns)
        .map(|column| column > 0 && column % 8 == 0)
        .collect()
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}
