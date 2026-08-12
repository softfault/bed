//! Terminal UI state, key dispatch, and frame rendering.
//!
//! Rendering is full-frame and uses xterm-compatible control sequences. Text
//! layout is based on extended grapheme clusters and terminal display cells,
//! keeping byte offsets and screen columns out of the platform boundary.
//!
//! References:
//! - xterm [`ctlseqs`](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)
//! - Unicode Standard Annex #29, [Text Segmentation](https://www.unicode.org/reports/tr29/)

#![forbid(unsafe_code)]

mod file_tree;
mod layout;

use anyhow::{Context, Result};
use bed_core::{
    BufferId, Document, Editor, RegexPattern, SubstituteOptions, SubstituteRange, ViewId,
};
use bed_terminal::{Key, SpecialKey, TerminalSize};
use file_tree::{FileTree, TreeEntryKind};
use layout::{Direction, Layout, Rect, ResizeAmount, SplitAxis, WindowId, window_in_direction};
use std::{collections::HashMap, path::PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// Editor policy rather than a terminal ABI value: tabs advance to four-column
// stops and are rendered as spaces to keep horizontal clipping deterministic.
const TAB_STOP: usize = 4;

// Full-frame rendering hides the cursor, clears the display, draws windows by
// absolute position, then restores the cursor at its final position.
const BEGIN_FRAME: &[u8] = b"\x1b[?25l\x1b[H\x1b[2J";
const REVERSE_VIDEO: &[u8] = b"\x1b[7m";
const RESET_STYLE: &[u8] = b"\x1b[m";
const VERTICAL_SEPARATOR: &[u8] = "│".as_bytes();
const TABLINE_ROWS: usize = 1;
const DEFAULT_FILE_TREE_WIDTH: usize = 20;
const MIN_FILE_TREE_WIDTH: usize = 10;
const MIN_EDITOR_WIDTH: usize = 12;
const FILE_TREE_HIDE_COLUMNS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Search,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Go,
    Delete,
    Yank,
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command<'a> {
    Empty,
    Write {
        force: bool,
    },
    WriteAll {
        force: bool,
    },
    Quit {
        force: bool,
    },
    WriteQuit {
        force: bool,
    },
    WriteQuitAll {
        force: bool,
    },
    NextBuffer,
    PreviousBuffer,
    Buffer(Option<&'a str>),
    DeleteBuffer {
        force: bool,
        number: Option<&'a str>,
    },
    Edit(Option<&'a str>),
    ListBuffers,
    Split(SplitAxis, Option<&'a str>),
    CloseWindow,
    OnlyWindow,
    Wincmd(Option<&'a str>),
    Resize(SplitAxis, Option<&'a str>),
    NewTab(Option<&'a str>),
    CloneTab,
    RenameTab(Option<&'a str>),
    NextTab(Option<&'a str>),
    PreviousTab,
    MoveTab(Option<&'a str>),
    CloseTab,
    OnlyTab,
    Tree(Option<&'a str>),
    TreeWidth(Option<&'a str>),
    RefreshTree,
    Substitute {
        range: SubstituteRange,
        expression: &'a str,
    },
    Unknown(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Register {
    Character(Vec<u8>),
    Line(Vec<u8>),
}

#[derive(Clone, Debug)]
struct Viewport {
    row_offset: usize,
    column_offset: usize,
    rows: usize,
}

#[derive(Debug)]
struct Window {
    view_id: ViewId,
    views: HashMap<BufferId, ViewId>,
    viewports: HashMap<BufferId, Viewport>,
}

#[derive(Debug)]
struct TabPage {
    id: TabId,
    automatic_title: String,
    title: Option<String>,
    layout: Layout,
    active_window: WindowId,
    window_history: Vec<WindowId>,
    file_tree: FileTree,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TabId(u64);

impl Window {
    fn new(buffer_id: BufferId, view_id: ViewId, viewport: Viewport) -> Self {
        Self {
            view_id,
            views: HashMap::from([(buffer_id, view_id)]),
            viewports: HashMap::from([(buffer_id, viewport)]),
        }
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            row_offset: 0,
            column_offset: 0,
            rows: 1,
        }
    }
}

#[derive(Debug)]
pub struct App {
    editor: Editor,
    mode: Mode,
    command: String,
    search: String,
    last_search: Option<RegexPattern>,
    message: String,
    pending: Option<Pending>,
    count: Option<usize>,
    register: Option<Register>,
    windows: HashMap<WindowId, Window>,
    layout: Layout,
    active_window: WindowId,
    next_window_id: u64,
    parked_tabs: Vec<Option<TabPage>>,
    active_tab: usize,
    active_tab_id: TabId,
    active_tab_automatic_title: String,
    active_tab_title: Option<String>,
    active_window_history: Vec<WindowId>,
    next_tab_id: u64,
    tab_history: Vec<TabId>,
    last_size: TerminalSize,
    file_tree: FileTree,
    file_tree_width: usize,
    insert_changed: bool,
    insert_back_on_unchanged: bool,
    should_quit: bool,
}

impl App {
    pub fn new(editor: Editor) -> Self {
        let active_tab_automatic_title = document_label(editor.document());
        let tree_root = editor
            .document()
            .path()
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let active_window = WindowId(0);
        let windows = HashMap::from([(
            active_window,
            Window::new(editor.buffer_id(), editor.view_id(), Viewport::default()),
        )]);
        Self {
            editor,
            mode: Mode::Normal,
            command: String::new(),
            search: String::new(),
            last_search: None,
            message: String::from("i: insert  :w: save  :q: quit"),
            pending: None,
            count: None,
            register: None,
            windows,
            layout: Layout::Window(active_window),
            active_window,
            next_window_id: 1,
            parked_tabs: vec![None],
            active_tab: 0,
            active_tab_id: TabId(0),
            active_tab_automatic_title,
            active_tab_title: None,
            active_window_history: Vec::new(),
            next_tab_id: 1,
            tab_history: Vec::new(),
            last_size: TerminalSize {
                rows: 3,
                columns: 1,
            },
            file_tree: FileTree::new(tree_root),
            file_tree_width: DEFAULT_FILE_TREE_WIDTH,
            insert_changed: false,
            insert_back_on_unchanged: false,
            should_quit: false,
        }
    }

    pub fn for_directory(editor: Editor, directory: PathBuf) -> Self {
        let mut app = Self::new(editor);
        app.file_tree = FileTree::new(directory);
        app.active_tab_automatic_title = app.file_tree.root_label();
        app.mode = Mode::Tree;
        app
    }

    pub fn handle_key(&mut self, key: Key) -> Result<()> {
        self.message.clear();
        match self.mode {
            Mode::Normal => self.handle_normal_key(key)?,
            Mode::Insert => self.handle_insert_key(key)?,
            Mode::Command => self.handle_command_key(key)?,
            Mode::Search => self.handle_search_key(key),
            Mode::Tree => self.handle_tree_key(key),
        }
        Ok(())
    }

    pub fn render(&mut self, size: TerminalSize) -> Vec<u8> {
        self.last_size = size;
        let rows = size.rows.max(3);
        let columns = size.columns.max(1);
        if self.mode == Mode::Tree && visible_file_tree_width(columns, self.file_tree_width) == 0 {
            self.mode = Mode::Normal;
            self.message
                .push_str("File tree hidden: terminal is too narrow");
        }
        let active_window = self.active_window;
        let active_view = self.active_window().view_id;
        let editor_area = self.editor_area();
        let rectangles = self.layout.rectangles(editor_area);
        let mut output = Vec::new();
        output.extend_from_slice(BEGIN_FRAME);
        if editor_area.columns > 0 {
            move_to(&mut output, 1, editor_area.column + 1);
            self.render_tabline(&mut output, editor_area.columns);
        }
        let tree_cursor = self.render_file_tree(&mut output, rows, columns);
        let mut editor_cursor = (1, 1);
        for (window_id, area) in rectangles {
            let view_id = self
                .windows
                .get(&window_id)
                .expect("layout references a missing window")
                .view_id;
            let switched = self.editor.switch_view(view_id);
            debug_assert!(switched);
            self.render_window(&mut output, window_id, area);
            if window_id == active_window {
                editor_cursor = self.window_cursor(area);
            }
        }
        let switched = self.editor.switch_view(active_view);
        debug_assert!(switched);

        let prompt = match self.mode {
            Mode::Command => format!(":{}", self.command),
            Mode::Search => format!("/{}", self.search),
            Mode::Normal | Mode::Insert | Mode::Tree => self.message.clone(),
        };
        let prompt_width = display_width(&prompt);
        // Reserve the last cell for the command cursor and horizontally scroll
        // only command mode; transient messages simply clip at the right edge.
        let prompt_offset = if matches!(self.mode, Mode::Command | Mode::Search) {
            prompt_width.saturating_sub(columns.saturating_sub(1))
        } else {
            0
        };
        move_to(&mut output, rows, 1);
        output.extend_from_slice(render_text(prompt.as_bytes(), prompt_offset, columns).as_bytes());

        let (cursor_row, cursor_column) = match self.mode {
            Mode::Command | Mode::Search => (rows, prompt_width.saturating_sub(prompt_offset) + 1),
            Mode::Tree => tree_cursor,
            Mode::Normal | Mode::Insert => editor_cursor,
        };
        output.extend_from_slice(
            // CSI H positions the cursor; xterm private mode 25 makes it visible
            // only after the complete frame has been emitted.
            format!(
                "\x1b[{};{}H\x1b[?25h",
                cursor_row.min(rows),
                cursor_column.min(columns)
            )
            .as_bytes(),
        );
        output
    }

    fn render_window(&mut self, output: &mut Vec<u8>, window_id: WindowId, area: Rect) {
        if area.columns == 0 || area.rows == 0 {
            return;
        }
        let text_rows = area.rows.saturating_sub(1);
        let line_number_width =
            line_number_width(self.editor.document().line_count(), area.columns);
        let text_columns = area.columns - line_number_width;
        let viewport_rows = text_rows.max(1);
        self.window_viewport_mut(window_id).rows = viewport_rows;
        self.scroll_window(window_id, viewport_rows, text_columns);

        let viewport = self.window_viewport(window_id).clone();
        for screen_row in 0..text_rows {
            move_to(output, area.row + screen_row + 1, area.column + 1);
            let document_row = viewport.row_offset + screen_row;
            let line = self.editor.document().line(document_row);
            if line_number_width > 0 {
                output.extend_from_slice(
                    render_line_number(document_row, line.is_some(), line_number_width).as_bytes(),
                );
            }
            if let Some(line) = line {
                output.extend_from_slice(
                    render_text(line, viewport.column_offset, text_columns).as_bytes(),
                );
            } else if line_number_width == 0 {
                output.push(b'~');
            }
        }

        move_to(output, area.row + area.rows, area.column + 1);
        output.extend_from_slice(REVERSE_VIDEO);
        output.extend_from_slice(
            self.status_line(area.columns, window_id == self.active_window)
                .as_bytes(),
        );
        output.extend_from_slice(RESET_STYLE);

        if area.column > 0 {
            for row in area.row..area.row + area.rows {
                move_to(output, row + 1, area.column);
                output.extend_from_slice(VERTICAL_SEPARATOR);
            }
        }
    }

    fn render_file_tree(
        &mut self,
        output: &mut Vec<u8>,
        rows: usize,
        columns: usize,
    ) -> (usize, usize) {
        let width = visible_file_tree_width(columns, self.file_tree_width);
        if width == 0 {
            return (1, 1);
        }
        let panel_row = 0;
        let panel_rows = rows - 1;
        move_to(output, panel_row + 1, 1);
        output.extend_from_slice(REVERSE_VIDEO);
        output.extend_from_slice(
            render_text(self.file_tree.root_label().as_bytes(), 0, width).as_bytes(),
        );
        output.extend_from_slice(RESET_STYLE);
        move_to(output, 1, width + 1);
        output.extend_from_slice(VERTICAL_SEPARATOR);

        let entry_rows = panel_rows.saturating_sub(1);
        self.file_tree.ensure_visible(entry_rows.max(1));
        let selected = self.file_tree.selected();
        let offset = self.file_tree.row_offset();
        for screen_row in 0..entry_rows {
            let index = offset + screen_row;
            let Some(entry) = self.file_tree.entries().get(index) else {
                break;
            };
            let marker = match entry.kind {
                TreeEntryKind::Parent | TreeEntryKind::File => "  ",
                TreeEntryKind::Directory if self.file_tree.is_expanded(&entry.path) => "- ",
                TreeEntryKind::Directory => "+ ",
            };
            let name = match entry.kind {
                TreeEntryKind::Parent => "..".into(),
                TreeEntryKind::Directory | TreeEntryKind::File => entry
                    .path
                    .file_name()
                    .unwrap_or_else(|| entry.path.as_os_str())
                    .to_string_lossy(),
            };
            let line = format!("{}{}{}", "  ".repeat(entry.depth), marker, name);
            move_to(output, panel_row + screen_row + 2, 1);
            if self.mode == Mode::Tree && index == selected {
                output.extend_from_slice(REVERSE_VIDEO);
            }
            output.extend_from_slice(render_text(line.as_bytes(), 0, width).as_bytes());
            if self.mode == Mode::Tree && index == selected {
                output.extend_from_slice(RESET_STYLE);
            }
        }

        let cursor_row = panel_row + selected.saturating_sub(offset) + 2;
        (cursor_row.min(rows - 1), 1)
    }

    fn window_cursor(&self, area: Rect) -> (usize, usize) {
        let text_rows = area.rows.saturating_sub(1);
        if area.columns == 0 || text_rows == 0 {
            return (area.row + 1, area.column + 1);
        }
        let line_number_width =
            line_number_width(self.editor.document().line_count(), area.columns);
        let viewport = self
            .windows
            .get(&self.active_window)
            .and_then(|window| window.viewports.get(&self.editor.buffer_id()))
            .expect("active editor view is missing its viewport");
        let (row, _) = self.editor.position();
        let column = self.cursor_display_column();
        (
            area.row + row.saturating_sub(viewport.row_offset) + 1,
            area.column + line_number_width + column.saturating_sub(viewport.column_offset) + 1,
        )
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    fn handle_normal_key(&mut self, key: Key) -> Result<()> {
        if self.capture_count(&key) {
            return Ok(());
        }
        if let Some(pending) = self.pending.take() {
            match (pending, &key) {
                (Pending::Delete, Key::Char('d')) => {
                    self.count = None;
                    self.delete_line();
                    return Ok(());
                }
                (Pending::Delete, Key::Char('w')) => {
                    self.count = None;
                    self.delete_word();
                    return Ok(());
                }
                (Pending::Delete, Key::Char('$')) => {
                    self.count = None;
                    self.delete_line_end();
                    return Ok(());
                }
                (Pending::Yank, Key::Char('y')) => {
                    self.count = None;
                    self.yank_line();
                    return Ok(());
                }
                (Pending::Yank, Key::Char('w')) => {
                    self.count = None;
                    self.yank_word();
                    return Ok(());
                }
                (Pending::Yank, Key::Char('$')) => {
                    self.count = None;
                    self.yank_line_end();
                    return Ok(());
                }
                (Pending::Go, Key::Char('g')) => {
                    self.count = None;
                    self.editor.move_to_first_line();
                    return Ok(());
                }
                (Pending::Go, Key::Char('t')) => {
                    if let Some(number) = self.count.take() {
                        self.switch_tab_position(number);
                    } else {
                        self.next_tab();
                    }
                    return Ok(());
                }
                (Pending::Go, Key::Char('T')) => {
                    let count = self.count.take().unwrap_or(1);
                    for _ in 0..count {
                        self.previous_tab();
                    }
                    return Ok(());
                }
                (Pending::Window, key) => {
                    let count = self.count.take();
                    self.execute_window_key(key, count, false);
                    return Ok(());
                }
                _ => {}
            }
        }

        if key == Key::Tab {
            if let Some(number) = self.count.take() {
                self.switch_tab_position(number);
            } else {
                self.next_tab();
            }
            return Ok(());
        }
        if key == Key::BackTab {
            let count = self.count.take().unwrap_or(1);
            for _ in 0..count {
                self.previous_tab();
            }
            return Ok(());
        }

        let count = if matches!(
            key,
            Key::Char('g') | Key::Char('d') | Key::Char('y') | Key::Ctrl('w')
        ) {
            1
        } else {
            self.count.take().unwrap_or(1)
        };

        match key {
            Key::Char('h') | Key::ArrowLeft | Key::Modified(SpecialKey::ArrowLeft, _) => {
                for _ in 0..count {
                    self.editor.move_left();
                }
            }
            Key::Char('j') | Key::ArrowDown | Key::Modified(SpecialKey::ArrowDown, _) => {
                for _ in 0..count {
                    self.editor.move_down(false);
                }
            }
            Key::Char('k') | Key::ArrowUp | Key::Modified(SpecialKey::ArrowUp, _) => {
                for _ in 0..count {
                    self.editor.move_up(false);
                }
            }
            Key::Char('l') | Key::ArrowRight | Key::Modified(SpecialKey::ArrowRight, _) => {
                for _ in 0..count {
                    self.editor.move_right(false);
                }
            }
            Key::Char('0') | Key::Home | Key::Modified(SpecialKey::Home, _) => {
                self.editor.move_line_start();
            }
            Key::Char('$') | Key::End | Key::Modified(SpecialKey::End, _) => {
                self.editor.move_line_end(false);
            }
            Key::Char('w') => {
                for _ in 0..count {
                    self.editor.move_word_forward();
                }
            }
            Key::Char('b') => {
                for _ in 0..count {
                    self.editor.move_word_backward();
                }
            }
            Key::Char('e') => {
                for _ in 0..count {
                    self.editor.move_word_end();
                }
            }
            Key::Char('g') => self.pending = Some(Pending::Go),
            Key::Char('G') => self.editor.move_to_last_line(),
            Key::Char('d') => self.pending = Some(Pending::Delete),
            Key::Char('y') => self.pending = Some(Pending::Yank),
            Key::Char('x') | Key::Delete | Key::Modified(SpecialKey::Delete, _) => {
                if self.editor.can_delete_char() {
                    self.editor.checkpoint();
                    if let Some(bytes) = self.editor.delete_char() {
                        self.register = Some(Register::Character(bytes));
                    }
                }
            }
            Key::Char('p') => self.put_register(false)?,
            Key::Char('P') => self.put_register(true)?,
            Key::Char('u') => {
                if self.editor.undo() {
                    self.editor.normalize_normal_cursor();
                } else {
                    self.message.push_str("Already at oldest change");
                }
            }
            Key::Ctrl('r') => {
                if self.editor.redo() {
                    self.editor.normalize_normal_cursor();
                } else {
                    self.message.push_str("Already at newest change");
                }
            }
            Key::Char('i') => self.enter_insert(false),
            Key::Char('a') => {
                self.editor.move_right(true);
                self.enter_insert(true);
            }
            Key::Char('I') => {
                self.editor.move_line_start();
                self.enter_insert(false);
            }
            Key::Char('A') => {
                self.editor.move_line_end(true);
                self.enter_insert(true);
            }
            Key::Char('o') => {
                self.editor.checkpoint();
                self.editor.open_line_below()?;
                self.mode = Mode::Insert;
                self.insert_changed = true;
                self.insert_back_on_unchanged = false;
            }
            Key::Char('O') => {
                self.editor.checkpoint();
                self.editor.open_line_above()?;
                self.mode = Mode::Insert;
                self.insert_changed = true;
                self.insert_back_on_unchanged = false;
            }
            Key::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
            }
            Key::Char('/') => {
                self.mode = Mode::Search;
                self.search.clear();
            }
            Key::Char('n') => self.repeat_search(false),
            Key::Char('N') => self.repeat_search(true),
            Key::PageUp | Key::Modified(SpecialKey::PageUp, _) => {
                self.move_page(false, self.viewport().rows);
            }
            Key::PageDown | Key::Modified(SpecialKey::PageDown, _) => {
                self.move_page(true, self.viewport().rows);
            }
            Key::Ctrl('u') => self.move_page(false, (self.viewport().rows / 2).max(1)),
            Key::Ctrl('d') => self.move_page(true, (self.viewport().rows / 2).max(1)),
            Key::Ctrl('w') => self.pending = Some(Pending::Window),
            Key::Ctrl('n') => {
                if self.file_tree_width() > 0 {
                    self.mode = Mode::Tree;
                } else {
                    self.message
                        .push_str("File tree hidden: terminal is too narrow");
                }
            }
            Key::Ctrl('s') => {
                self.save(false);
            }
            Key::Paste(_) => self
                .message
                .push_str("Paste is only accepted in insert mode"),
            Key::Escape => self.pending = None,
            _ => {}
        }
        Ok(())
    }

    fn handle_insert_key(&mut self, key: Key) -> Result<()> {
        match key {
            Key::Escape | Key::Ctrl('c') => {
                if self.insert_changed || self.insert_back_on_unchanged {
                    self.editor.move_left();
                }
                self.editor.normalize_normal_cursor();
                self.mode = Mode::Normal;
                self.insert_back_on_unchanged = false;
            }
            Key::Enter => {
                self.begin_insert_change();
                self.editor.insert_newline()?;
            }
            Key::Tab => {
                self.begin_insert_change();
                self.editor.insert(b'\t')?;
            }
            Key::Paste(text) => {
                if !text.is_empty() {
                    self.begin_insert_change();
                    self.editor.insert_paste(&text)?;
                }
            }
            Key::Backspace => {
                if self.editor.cursor().offset() > 0 {
                    self.begin_insert_change();
                    self.editor.backspace();
                }
            }
            Key::Delete | Key::Modified(SpecialKey::Delete, _) => {
                if self.editor.cursor().offset() < self.editor.document().len() {
                    self.begin_insert_change();
                    self.editor.delete_forward();
                }
            }
            Key::ArrowLeft | Key::Modified(SpecialKey::ArrowLeft, _) => {
                self.editor.move_left();
            }
            Key::ArrowRight | Key::Modified(SpecialKey::ArrowRight, _) => {
                self.editor.move_right(true);
            }
            Key::ArrowUp | Key::Modified(SpecialKey::ArrowUp, _) => {
                self.editor.move_up(true);
            }
            Key::ArrowDown | Key::Modified(SpecialKey::ArrowDown, _) => {
                self.editor.move_down(true);
            }
            Key::Home | Key::Modified(SpecialKey::Home, _) => self.editor.move_line_start(),
            Key::End | Key::Modified(SpecialKey::End, _) => self.editor.move_line_end(true),
            Key::Char(character) if !character.is_control() => {
                self.begin_insert_change();
                let mut bytes = [0; 4];
                self.editor
                    .insert_bytes(character.encode_utf8(&mut bytes).as_bytes())?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_command_key(&mut self, key: Key) -> Result<()> {
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.mode = Mode::Normal;
                self.command.clear();
            }
            Key::Backspace => {
                if self.command.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            Key::Enter => {
                let command = std::mem::take(&mut self.command);
                self.mode = Mode::Normal;
                self.execute_command(command.trim());
            }
            Key::Char(character) if !character.is_control() => self.command.push(character),
            _ => {}
        }
        Ok(())
    }

    fn handle_search_key(&mut self, key: Key) {
        match key {
            Key::Escape | Key::Ctrl('c') => {
                self.mode = Mode::Normal;
                self.search.clear();
            }
            Key::Backspace => {
                if self.search.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            Key::Enter => {
                let query = std::mem::take(&mut self.search);
                self.mode = Mode::Normal;
                if query.is_empty() {
                    return;
                }
                match RegexPattern::compile(&query) {
                    Ok(pattern) => {
                        if !self.editor.search_forward(&pattern) {
                            self.message
                                .push_str(&format!("Pattern not found: {query}"));
                        }
                        self.last_search = Some(pattern);
                    }
                    Err(error) => self.message.push_str(&format!("Search failed: {error:#}")),
                }
            }
            Key::Char(character) if !character.is_control() => self.search.push(character),
            _ => {}
        }
    }

    fn handle_tree_key(&mut self, key: Key) {
        if self.capture_count(&key) {
            return;
        }
        if self.pending.take() == Some(Pending::Window) {
            let count = self.count.take();
            self.execute_window_key(&key, count, true);
            return;
        }
        let count = if key == Key::Ctrl('w') {
            1
        } else {
            self.count.take().unwrap_or(1)
        };
        match key {
            Key::Escape | Key::Ctrl('c') | Key::Ctrl('n') | Key::Char('q') => {
                self.mode = Mode::Normal;
            }
            Key::Char('j') | Key::ArrowDown => {
                for _ in 0..count {
                    self.file_tree.move_down();
                }
            }
            Key::Char('k') | Key::ArrowUp => {
                for _ in 0..count {
                    self.file_tree.move_up();
                }
            }
            Key::Char('h') | Key::ArrowLeft => {
                if let Err(error) = self.file_tree.collapse() {
                    self.message.push_str(&format!("Tree failed: {error}"));
                }
            }
            Key::Char('l') | Key::ArrowRight | Key::Enter => match self.file_tree.activate() {
                Ok(Some(path)) => match self.editor.open_buffer(path) {
                    Ok(buffer_id) => {
                        self.show_buffer(buffer_id);
                        self.mode = Mode::Normal;
                    }
                    Err(error) => self.message.push_str(&format!("Open failed: {error:#}")),
                },
                Ok(None) => {}
                Err(error) => self.message.push_str(&format!("Tree failed: {error}")),
            },
            Key::Char('r') => {
                if let Err(error) = self.file_tree.refresh() {
                    self.message.push_str(&format!("Tree failed: {error}"));
                }
            }
            Key::Ctrl('w') => self.pending = Some(Pending::Window),
            _ => {}
        }
    }

    fn execute_command(&mut self, command: &str) {
        match parse_command(command) {
            Command::Write { force } => {
                self.save(force);
            }
            Command::WriteAll { force } => {
                self.save_all(force);
            }
            Command::Quit { force: true } => self.should_quit = true,
            Command::Quit { force: false } => self.quit_if_clean(),
            Command::WriteQuit { force } => {
                if self.save(force) {
                    self.quit_if_clean();
                }
            }
            Command::WriteQuitAll { force } => {
                if self.save_all(force) {
                    self.quit_if_clean();
                }
            }
            Command::NextBuffer => {
                if let Some(buffer_id) = self.relative_buffer(1) {
                    self.show_buffer(buffer_id);
                } else {
                    self.message.push_str("No other buffer");
                }
            }
            Command::PreviousBuffer => {
                if let Some(buffer_id) = self.relative_buffer(-1) {
                    self.show_buffer(buffer_id);
                } else {
                    self.message.push_str("No other buffer");
                }
            }
            Command::Buffer(Some(number)) => self.switch_buffer_number(number),
            Command::Buffer(None) => self.describe_active_buffer(),
            Command::DeleteBuffer { force, number } => self.delete_buffer(number, force),
            Command::Edit(Some(path)) => self.edit_path(path),
            Command::Edit(None) => self.message.push_str("File path required"),
            Command::ListBuffers => self.list_buffers(),
            Command::Split(axis, path) => self.split_window(axis, path),
            Command::CloseWindow => self.close_active_window(),
            Command::OnlyWindow => self.keep_only_active_window(),
            Command::Wincmd(Some(command)) => self.execute_window_command(command),
            Command::Wincmd(None) => self.message.push_str("Window command required"),
            Command::Resize(axis, Some(value)) => self.resize_from_command(axis, value),
            Command::Resize(_, None) => self.message.push_str("Window size required"),
            Command::NewTab(path) => self.new_tab(path),
            Command::CloneTab => self.clone_tab(),
            Command::RenameTab(title) => self.rename_tab(title),
            Command::NextTab(Some(number)) => self.switch_tab_number(number),
            Command::NextTab(None) => self.next_tab(),
            Command::PreviousTab => self.previous_tab(),
            Command::MoveTab(position) => self.move_tab(position),
            Command::CloseTab => self.close_tab(),
            Command::OnlyTab => self.keep_only_active_tab(),
            Command::Tree(root) => self.focus_tree(root),
            Command::TreeWidth(Some(value)) => self.set_tree_width_from_command(value),
            Command::TreeWidth(None) => self.message.push_str("File tree width required"),
            Command::RefreshTree => self.refresh_tree(),
            Command::Substitute { range, expression } => self.execute_substitute(range, expression),
            Command::Empty => {}
            Command::Unknown(command) => self
                .message
                .push_str(&format!("Not an editor command: {command}")),
        }
    }

    fn quit_if_clean(&mut self) {
        match self.editor.dirty_buffer_count() {
            0 => self.should_quit = true,
            count => {
                if !self.message.is_empty() {
                    self.message.push_str("; ");
                }
                if count == 1 {
                    self.message
                        .push_str("No write since last change (use :q! to discard)");
                } else {
                    self.message.push_str(&format!(
                        "{count} buffers have unsaved changes (use :q! to discard)"
                    ));
                }
            }
        }
    }

    fn switch_buffer_number(&mut self, number: &str) {
        let Ok(number) = number.parse::<usize>() else {
            self.message.push_str("Buffer number must be an integer");
            return;
        };
        let Some(buffer_id) = self.editor.buffer_id_at(number) else {
            self.message
                .push_str(&format!("Buffer {number} does not exist"));
            return;
        };
        self.show_buffer(buffer_id);
    }

    fn edit_path(&mut self, path: &str) {
        match self.editor.open_buffer(PathBuf::from(path)) {
            Ok(buffer_id) => self.show_buffer(buffer_id),
            Err(error) => self.message.push_str(&format!("Open failed: {error:#}")),
        }
    }

    fn focus_tree(&mut self, root: Option<&str>) {
        if let Some(root) = root
            && let Err(error) = self.file_tree.set_root(PathBuf::from(root))
        {
            self.message.push_str(&format!("Tree failed: {error}"));
            return;
        }
        if self.file_tree_width() == 0 {
            self.message
                .push_str("File tree hidden: terminal is too narrow");
        } else {
            self.mode = Mode::Tree;
        }
    }

    fn refresh_tree(&mut self) {
        if let Err(error) = self.file_tree.refresh() {
            self.message.push_str(&format!("Tree failed: {error}"));
        }
    }

    fn resize_from_command(&mut self, axis: SplitAxis, value: &str) {
        match parse_resize_amount(value) {
            Some(amount) => self.resize_active_window(axis, amount),
            None => self.message.push_str("Window size must be an integer"),
        }
    }

    fn set_tree_width_from_command(&mut self, value: &str) {
        let Ok(width) = value.parse::<usize>() else {
            self.message.push_str("File tree width must be an integer");
            return;
        };
        if width < MIN_FILE_TREE_WIDTH {
            self.message.push_str(&format!(
                "File tree width must be at least {MIN_FILE_TREE_WIDTH}"
            ));
            return;
        }
        self.file_tree_width = width;
    }

    fn delete_buffer(&mut self, number: Option<&str>, force: bool) {
        let buffer_id = match number {
            Some(number) => {
                let Ok(number) = number.parse::<usize>() else {
                    self.message.push_str("Buffer number must be an integer");
                    return;
                };
                let Some(buffer_id) = self.editor.buffer_id_at(number) else {
                    self.message
                        .push_str(&format!("Buffer {number} does not exist"));
                    return;
                };
                buffer_id
            }
            None => self.editor.buffer_id(),
        };

        let dirty = self
            .editor
            .buffers()
            .get(buffer_id)
            .expect("buffer order references a missing buffer")
            .document()
            .is_dirty();
        if dirty && !force {
            self.message
                .push_str("No write since last change (use :bdelete! to discard)");
            return;
        }
        if !self.editor.close_buffer(buffer_id) {
            self.message.push_str("Cannot delete the last buffer");
            return;
        }
        let affected: Vec<_> = self
            .windows
            .iter()
            .filter(|(_, window)| window.views.get(&buffer_id) == Some(&window.view_id))
            .map(|(&window_id, _)| window_id)
            .collect();
        for window in self.windows.values_mut() {
            window.views.remove(&buffer_id);
            window.viewports.remove(&buffer_id);
        }
        let replacement_buffer = self.editor.buffer_id();
        let replacement_view = self.editor.view_id();
        for window_id in affected {
            let existing = self
                .windows
                .get(&window_id)
                .and_then(|window| window.views.get(&replacement_buffer))
                .copied();
            let view_id = if let Some(view_id) = existing {
                view_id
            } else if window_id == self.active_window {
                replacement_view
            } else {
                self.editor
                    .duplicate_view(replacement_view)
                    .expect("replacement view disappeared while repairing windows")
            };
            let window = self
                .windows
                .get_mut(&window_id)
                .expect("layout references a missing window");
            window.view_id = view_id;
            window.views.insert(replacement_buffer, view_id);
            window.viewports.entry(replacement_buffer).or_default();
        }
        let active_view = self.active_window().view_id;
        let switched = self.editor.switch_view(active_view);
        debug_assert!(switched);
        self.ensure_active_viewport();
    }

    fn describe_active_buffer(&mut self) {
        self.message.push_str(&format!(
            "{}: {}",
            self.editor.buffer_number(),
            self.editor.document().path().display()
        ));
    }

    fn list_buffers(&mut self) {
        let active = self.editor.buffer_id();
        for (index, &buffer_id) in self.editor.buffer_ids().iter().enumerate() {
            if index > 0 {
                self.message.push_str("  ");
            }
            let buffer = self
                .editor
                .buffers()
                .get(buffer_id)
                .expect("buffer order references a missing buffer");
            let current = if buffer_id == active { '%' } else { ' ' };
            let dirty = if buffer.document().is_dirty() {
                " [+]"
            } else {
                ""
            };
            self.message.push_str(&format!(
                "{}:{current} {}{dirty}",
                index + 1,
                buffer.document().path().display()
            ));
        }
    }

    fn ensure_active_viewport(&mut self) {
        let buffer_id = self.editor.buffer_id();
        let view_id = self.editor.view_id();
        let window = self.active_window_mut();
        window.view_id = view_id;
        window.views.insert(buffer_id, view_id);
        window.viewports.entry(buffer_id).or_default();
    }

    fn split_window(&mut self, axis: SplitAxis, path: Option<&str>) {
        let inherited_viewport = self.viewport().clone();
        let (source, buffer_id, viewport) = if let Some(path) = path {
            let buffer_id = match self.editor.open_buffer(PathBuf::from(path)) {
                Ok(buffer_id) => buffer_id,
                Err(error) => {
                    self.message.push_str(&format!("Open failed: {error:#}"));
                    return;
                }
            };
            (self.editor.view_id(), buffer_id, Viewport::default())
        } else {
            (
                self.active_window().view_id,
                self.editor.buffer_id(),
                inherited_viewport,
            )
        };
        let view_id = self
            .editor
            .duplicate_view(source)
            .expect("active window references a missing editor view");
        let window_id = self.allocate_window_id();
        let inserted = self.layout.split(self.active_window, window_id, axis);
        debug_assert!(inserted);
        self.windows
            .insert(window_id, Window::new(buffer_id, view_id, viewport));
        self.activate_window(window_id);
    }

    fn close_active_window(&mut self) {
        if self.layout.windows().len() == 1 {
            if self.parked_tabs.len() > 1 {
                self.close_tab();
            } else {
                self.message.push_str("Cannot close the last window");
            }
            return;
        }

        let closing = self.active_window;
        let layout = self
            .layout
            .clone()
            .remove(closing)
            .expect("closing a non-final window produced an empty layout");
        let remaining = layout.windows();
        self.active_window_history
            .retain(|window_id| remaining.contains(window_id));
        let next = self
            .active_window_history
            .pop()
            .unwrap_or_else(|| *remaining.first().expect("non-empty layout has no windows"));
        let window = self
            .windows
            .remove(&closing)
            .expect("layout references a missing window");
        self.layout = layout;
        self.active_window = next;
        let next_view = self.active_window().view_id;
        let switched = self.editor.switch_view(next_view);
        debug_assert!(switched);
        for view_id in window.views.into_values() {
            self.editor.remove_view(view_id);
        }
    }

    fn keep_only_active_window(&mut self) {
        let active = self.active_window;
        let closing: Vec<_> = self
            .layout
            .windows()
            .into_iter()
            .filter(|&window_id| window_id != active)
            .collect();
        for window_id in closing {
            if let Some(window) = self.windows.remove(&window_id) {
                for view_id in window.views.into_values() {
                    self.editor.remove_view(view_id);
                }
            }
        }
        self.layout = Layout::Window(active);
        self.active_window_history.clear();
    }

    fn render_tabline(&self, output: &mut Vec<u8>, columns: usize) {
        let mut labels = Vec::with_capacity(self.parked_tabs.len());
        for index in 0..self.parked_tabs.len() {
            let (automatic_title, title, layout) = if index == self.active_tab {
                (
                    self.active_tab_automatic_title.as_str(),
                    self.active_tab_title.as_deref(),
                    &self.layout,
                )
            } else {
                let tab = self.parked_tabs[index]
                    .as_ref()
                    .expect("inactive tab page is missing its layout");
                (
                    tab.automatic_title.as_str(),
                    tab.title.as_deref(),
                    &tab.layout,
                )
            };
            let active = index == self.active_tab;
            let dirty = if self.tab_has_dirty_buffer(layout) {
                "+"
            } else {
                ""
            };
            let name = title.unwrap_or(automatic_title);
            let label = if active {
                format!(" [{} {}{dirty}]", index + 1, name)
            } else {
                format!("  {} {name}{dirty}", index + 1)
            };
            labels.push((label, active));
        }

        let active_end = labels
            .iter()
            .take(self.active_tab + 1)
            .map(|(label, _)| display_width(label))
            .sum::<usize>();
        let offset = active_end.saturating_sub(columns);
        let end = offset.saturating_add(columns);
        let mut source_column = 0;
        for (label, active) in labels {
            let label_width = display_width(&label);
            let label_end = source_column + label_width;
            if label_end <= offset {
                source_column = label_end;
                continue;
            }
            if source_column >= end {
                break;
            }
            let label_offset = offset.saturating_sub(source_column);
            let visible_width = label_end.min(end) - source_column.max(offset);
            let rendered = render_text(label.as_bytes(), label_offset, visible_width);
            if active {
                output.extend_from_slice(REVERSE_VIDEO);
            }
            output.extend_from_slice(rendered.as_bytes());
            if active {
                output.extend_from_slice(RESET_STYLE);
            }
            source_column = label_end;
        }
    }

    fn tab_has_dirty_buffer(&self, layout: &Layout) -> bool {
        layout.windows().into_iter().any(|window_id| {
            let view_id = self
                .windows
                .get(&window_id)
                .expect("tab page references a missing window")
                .view_id;
            let buffer_id = self
                .editor
                .view_by_id(view_id)
                .expect("window references a missing editor view")
                .buffer_id();
            self.editor
                .buffers()
                .get(buffer_id)
                .expect("editor view references a missing buffer")
                .document()
                .is_dirty()
        })
    }

    fn execute_window_command(&mut self, command: &str) {
        match command {
            "h" => self.focus_window(Direction::Left),
            "j" => self.focus_window(Direction::Down),
            "k" => self.focus_window(Direction::Up),
            "l" => self.focus_window(Direction::Right),
            "w" => self.focus_next_window(),
            "<" => self.resize_active_window(SplitAxis::Columns, ResizeAmount::Decrease(1)),
            ">" => self.resize_active_window(SplitAxis::Columns, ResizeAmount::Increase(1)),
            "-" => self.resize_active_window(SplitAxis::Rows, ResizeAmount::Decrease(1)),
            "+" => self.resize_active_window(SplitAxis::Rows, ResizeAmount::Increase(1)),
            "|" => self.resize_active_window(SplitAxis::Columns, ResizeAmount::Exact(usize::MAX)),
            "_" => {
                self.resize_active_window(SplitAxis::Rows, ResizeAmount::Exact(usize::MAX));
            }
            "=" => self.layout.equalize(),
            _ => self.message.push_str("Invalid window command"),
        }
    }

    fn execute_window_key(&mut self, key: &Key, count: Option<usize>, tree: bool) {
        let amount = count.unwrap_or(1);
        if tree {
            if matches!(key, Key::Char('w') | Key::Ctrl('w'))
                || window_direction(key) == Some(Direction::Right)
            {
                self.mode = Mode::Normal;
                return;
            }
            if window_direction(key).is_some() {
                self.message.push_str("No window in that direction");
                return;
            }
            match key {
                Key::Escape => {}
                Key::Char('<') => self.resize_file_tree(ResizeAmount::Decrease(amount)),
                Key::Char('>') => self.resize_file_tree(ResizeAmount::Increase(amount)),
                Key::Char('|') => {
                    self.resize_file_tree(ResizeAmount::Exact(count.unwrap_or(usize::MAX)))
                }
                Key::Char('-') | Key::Char('+') | Key::Char('_') => {
                    self.message.push_str("File tree height is fixed");
                }
                Key::Char('=') => self.file_tree_width = DEFAULT_FILE_TREE_WIDTH,
                _ => self.message.push_str("Invalid file tree window command"),
            }
            return;
        }

        match key {
            Key::Escape => {}
            Key::Char('w') | Key::Ctrl('w') => {
                for _ in 0..amount {
                    self.focus_next_window();
                }
            }
            Key::Char('<') => {
                self.resize_active_window(SplitAxis::Columns, ResizeAmount::Decrease(amount))
            }
            Key::Char('>') => {
                self.resize_active_window(SplitAxis::Columns, ResizeAmount::Increase(amount))
            }
            Key::Char('-') => {
                self.resize_active_window(SplitAxis::Rows, ResizeAmount::Decrease(amount));
            }
            Key::Char('+') => {
                self.resize_active_window(SplitAxis::Rows, ResizeAmount::Increase(amount));
            }
            Key::Char('|') => self.resize_active_window(
                SplitAxis::Columns,
                ResizeAmount::Exact(count.unwrap_or(usize::MAX)),
            ),
            Key::Char('_') => self.resize_active_window(
                SplitAxis::Rows,
                ResizeAmount::Exact(count.unwrap_or(usize::MAX)),
            ),
            Key::Char('=') => self.layout.equalize(),
            _ => {
                if let Some(direction) = window_direction(key) {
                    for _ in 0..amount {
                        self.focus_window(direction);
                    }
                } else {
                    self.message.push_str("Invalid window command");
                }
            }
        }
    }

    fn resize_active_window(&mut self, axis: SplitAxis, amount: ResizeAmount) {
        let area = self.editor_area();
        if !self.layout.resize(self.active_window, axis, amount, area) {
            self.message.push_str(match axis {
                SplitAxis::Rows => "No horizontal split to resize",
                SplitAxis::Columns => "No vertical split to resize",
            });
        }
    }

    fn resize_file_tree(&mut self, amount: ResizeAmount) {
        let columns = self.last_size.columns.max(1);
        let Some(maximum) = maximum_file_tree_width(columns) else {
            self.message
                .push_str("File tree hidden: terminal is too narrow");
            return;
        };
        let current = self.file_tree_width();
        let requested = match amount {
            ResizeAmount::Increase(amount) => current.saturating_add(amount),
            ResizeAmount::Decrease(amount) => current.saturating_sub(amount),
            ResizeAmount::Exact(amount) => amount,
        };
        self.file_tree_width = requested.clamp(MIN_FILE_TREE_WIDTH, maximum);
    }

    fn capture_count(&mut self, key: &Key) -> bool {
        let Key::Char(character) = key else {
            return false;
        };
        let Some(digit) = character.to_digit(10) else {
            return false;
        };
        if digit == 0 && self.count.is_none() {
            return false;
        }
        self.count = Some(
            self.count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit as usize),
        );
        true
    }

    fn focus_window(&mut self, direction: Direction) {
        let rectangles = self.layout.rectangles(self.editor_area());
        if let Some(window_id) = window_in_direction(&rectangles, self.active_window, direction) {
            self.activate_window(window_id);
        } else if direction == Direction::Left && self.file_tree_width() > 0 {
            self.mode = Mode::Tree;
        }
    }

    fn focus_next_window(&mut self) {
        let windows = self.layout.windows();
        let current = windows
            .iter()
            .position(|&window_id| window_id == self.active_window)
            .expect("active window is missing from the layout");
        self.activate_window(windows[(current + 1) % windows.len()]);
    }

    fn activate_window(&mut self, window_id: WindowId) {
        if window_id == self.active_window {
            return;
        }
        let view_id = self
            .windows
            .get(&window_id)
            .expect("layout references a missing window")
            .view_id;
        let switched = self.editor.switch_view(view_id);
        debug_assert!(switched);
        self.active_window_history
            .retain(|&candidate| candidate != self.active_window);
        self.active_window_history.push(self.active_window);
        self.active_window = window_id;
    }

    fn editor_area(&self) -> Rect {
        let tree_width = self.file_tree_width();
        let tree_separator = usize::from(tree_width > 0);
        Rect {
            row: TABLINE_ROWS,
            column: tree_width + tree_separator,
            rows: self.last_size.rows.max(3) - 1 - TABLINE_ROWS,
            columns: self
                .last_size
                .columns
                .max(1)
                .saturating_sub(tree_width + tree_separator),
        }
    }

    fn file_tree_width(&self) -> usize {
        visible_file_tree_width(self.last_size.columns.max(1), self.file_tree_width)
    }

    fn new_tab(&mut self, path: Option<&str>) {
        let inherited_tree = self.file_tree.clone();
        let inherited_viewport = self.viewport().clone();
        let (source, buffer_id, viewport) = if let Some(path) = path {
            let buffer_id = match self.editor.open_buffer(PathBuf::from(path)) {
                Ok(buffer_id) => buffer_id,
                Err(error) => {
                    self.message.push_str(&format!("Open failed: {error:#}"));
                    return;
                }
            };
            (self.editor.view_id(), buffer_id, Viewport::default())
        } else {
            (
                self.active_window().view_id,
                self.editor.buffer_id(),
                inherited_viewport,
            )
        };
        let view_id = self
            .editor
            .duplicate_view(source)
            .expect("active editor view disappeared while creating a tab");
        let window_id = self.allocate_window_id();
        self.windows
            .insert(window_id, Window::new(buffer_id, view_id, viewport));

        let next = TabPage {
            id: self.allocate_tab_id(),
            automatic_title: document_label(self.editor.document()),
            title: None,
            layout: Layout::Window(window_id),
            active_window: window_id,
            window_history: Vec::new(),
            file_tree: inherited_tree,
        };
        self.insert_tab_after_active(next);
    }

    fn clone_tab(&mut self) {
        let source_windows = self.layout.windows();
        let mut window_ids = HashMap::new();
        for source_id in source_windows {
            let source = self
                .windows
                .get(&source_id)
                .expect("layout references a missing window");
            let source_view = source.view_id;
            let source_views: Vec<_> = source
                .views
                .iter()
                .map(|(&buffer_id, &view_id)| (buffer_id, view_id))
                .collect();
            let viewports = source.viewports.clone();

            let mut views = HashMap::new();
            let mut active_view = None;
            for (buffer_id, view_id) in source_views {
                let duplicate = self
                    .editor
                    .duplicate_view(view_id)
                    .expect("window references a missing editor view");
                if view_id == source_view {
                    active_view = Some(duplicate);
                }
                views.insert(buffer_id, duplicate);
            }
            let window_id = self.allocate_window_id();
            self.windows.insert(
                window_id,
                Window {
                    view_id: active_view.expect("active window view is missing from its view map"),
                    views,
                    viewports,
                },
            );
            window_ids.insert(source_id, window_id);
        }

        let mut map_window = |window_id| {
            *window_ids
                .get(&window_id)
                .expect("cloned layout references an unmapped window")
        };
        let next = TabPage {
            id: self.allocate_tab_id(),
            automatic_title: self.active_tab_automatic_title.clone(),
            title: self.active_tab_title.clone(),
            layout: self.layout.map_windows(&mut map_window),
            active_window: map_window(self.active_window),
            window_history: self
                .active_window_history
                .iter()
                .filter_map(|window_id| window_ids.get(window_id).copied())
                .collect(),
            file_tree: self.file_tree.clone(),
        };
        self.insert_tab_after_active(next);
    }

    fn insert_tab_after_active(&mut self, next: TabPage) {
        let next_view = self
            .windows
            .get(&next.active_window)
            .expect("new tab references a missing window")
            .view_id;
        let previous_id = self.active_tab_id;
        let previous = self.replace_active_tab(next);
        let next_index = self.active_tab + 1;
        self.parked_tabs[self.active_tab] = Some(previous);
        self.parked_tabs.insert(next_index, None);
        self.active_tab = next_index;
        self.tab_history.retain(|&id| id != previous_id);
        self.tab_history.push(previous_id);
        let switched = self.editor.switch_view(next_view);
        debug_assert!(switched);
    }

    fn rename_tab(&mut self, title: Option<&str>) {
        self.active_tab_title = title.map(str::to_owned);
    }

    fn next_tab(&mut self) {
        if self.parked_tabs.len() < 2 {
            self.message.push_str("No other tab page");
            return;
        }
        self.switch_tab((self.active_tab + 1) % self.parked_tabs.len());
    }

    fn switch_tab_number(&mut self, number: &str) {
        let Ok(number) = number.parse::<usize>() else {
            self.message.push_str("Tab number must be an integer");
            return;
        };
        self.switch_tab_position(number);
    }

    fn switch_tab_position(&mut self, number: usize) {
        let Some(target) = number.checked_sub(1) else {
            self.message.push_str("Tab number must be at least 1");
            return;
        };
        if target >= self.parked_tabs.len() {
            self.message
                .push_str(&format!("Tab {number} does not exist"));
            return;
        }
        self.switch_tab(target);
    }

    fn previous_tab(&mut self) {
        if self.parked_tabs.len() < 2 {
            self.message.push_str("No other tab page");
            return;
        }
        let target = self
            .active_tab
            .checked_sub(1)
            .unwrap_or(self.parked_tabs.len() - 1);
        self.switch_tab(target);
    }

    fn move_tab(&mut self, position: Option<&str>) {
        let Some(position) = position else {
            self.message.push_str("Tab position required");
            return;
        };
        let Ok(position) = position.parse::<usize>() else {
            self.message.push_str("Tab position must be an integer");
            return;
        };
        let Some(target) = position.checked_sub(1) else {
            self.message.push_str("Tab position must be at least 1");
            return;
        };
        if target >= self.parked_tabs.len() {
            self.message
                .push_str(&format!("Tab position {position} does not exist"));
            return;
        }
        if target == self.active_tab {
            return;
        }
        let active = self.parked_tabs.remove(self.active_tab);
        debug_assert!(active.is_none());
        self.parked_tabs.insert(target, None);
        self.active_tab = target;
    }

    fn switch_tab(&mut self, target: usize) {
        if target == self.active_tab {
            return;
        }
        let next = self.parked_tabs[target]
            .take()
            .expect("inactive tab page is missing its layout");
        let previous_id = self.active_tab_id;
        let previous = self.replace_active_tab(next);
        self.parked_tabs[self.active_tab] = Some(previous);
        self.active_tab = target;
        self.tab_history.retain(|&id| id != previous_id);
        self.tab_history.push(previous_id);
        self.tab_history.retain(|&id| id != self.active_tab_id);
        let view_id = self.active_window().view_id;
        let switched = self.editor.switch_view(view_id);
        debug_assert!(switched);
    }

    fn close_tab(&mut self) {
        if self.parked_tabs.len() == 1 {
            self.message.push_str("Cannot close the last tab page");
            return;
        }
        let closing_index = self.active_tab;
        let removed = self.parked_tabs.remove(closing_index);
        debug_assert!(removed.is_none());
        let mut next_index = None;
        while let Some(id) = self.tab_history.pop() {
            if let Some(index) = self
                .parked_tabs
                .iter()
                .position(|tab| tab.as_ref().is_some_and(|candidate| candidate.id == id))
            {
                next_index = Some(index);
                break;
            }
        }
        let next_index =
            next_index.unwrap_or_else(|| closing_index.min(self.parked_tabs.len() - 1));
        let next = self.parked_tabs[next_index]
            .take()
            .expect("inactive tab page is missing its layout");
        let closing = self.replace_active_tab(next);
        self.active_tab = next_index;
        self.tab_history.retain(|&id| id != closing.id);
        let view_id = self.active_window().view_id;
        let switched = self.editor.switch_view(view_id);
        debug_assert!(switched);
        self.discard_windows(closing.layout.windows());
    }

    fn keep_only_active_tab(&mut self) {
        let closing_windows: Vec<_> = self
            .parked_tabs
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|tab| tab.layout.windows())
            .collect();
        self.discard_windows(closing_windows);
        self.parked_tabs = vec![None];
        self.active_tab = 0;
        self.tab_history.clear();
    }

    fn replace_active_tab(&mut self, next: TabPage) -> TabPage {
        TabPage {
            id: std::mem::replace(&mut self.active_tab_id, next.id),
            automatic_title: std::mem::replace(
                &mut self.active_tab_automatic_title,
                next.automatic_title,
            ),
            title: std::mem::replace(&mut self.active_tab_title, next.title),
            layout: std::mem::replace(&mut self.layout, next.layout),
            active_window: std::mem::replace(&mut self.active_window, next.active_window),
            window_history: std::mem::replace(&mut self.active_window_history, next.window_history),
            file_tree: std::mem::replace(&mut self.file_tree, next.file_tree),
        }
    }

    fn allocate_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id = self
            .next_tab_id
            .checked_add(1)
            .expect("tab ID space exhausted");
        id
    }

    fn allocate_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window ID space exhausted");
        id
    }

    fn discard_windows(&mut self, windows: Vec<WindowId>) {
        for window_id in windows {
            if let Some(window) = self.windows.remove(&window_id) {
                for view_id in window.views.into_values() {
                    self.editor.remove_view(view_id);
                }
            }
        }
    }

    fn show_buffer(&mut self, buffer_id: BufferId) {
        let existing = self.active_window().views.get(&buffer_id).copied();
        let view_id = existing.unwrap_or_else(|| {
            let primary = self
                .editor
                .view_for_buffer(buffer_id)
                .expect("open buffer is missing its primary view");
            if self.windows.iter().any(|(&window_id, window)| {
                window_id != self.active_window && window.view_id == primary
            }) {
                self.editor
                    .duplicate_view(primary)
                    .expect("primary view disappeared while duplicating it")
            } else {
                primary
            }
        });
        let switched = self.editor.switch_view(view_id);
        debug_assert!(switched);
        self.ensure_active_viewport();
    }

    fn relative_buffer(&self, delta: isize) -> Option<BufferId> {
        let count = self.editor.buffer_count();
        if count < 2 {
            return None;
        }
        let current = self.editor.buffer_number() - 1;
        let next = if delta < 0 {
            current
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(count - 1)
        } else {
            (current + delta as usize) % count
        };
        self.editor.buffer_ids().get(next).copied()
    }

    fn enter_insert(&mut self, back_on_unchanged: bool) {
        self.mode = Mode::Insert;
        self.insert_changed = false;
        self.insert_back_on_unchanged = back_on_unchanged;
    }

    fn delete_line(&mut self) {
        if self.editor.document().is_empty() {
            return;
        }
        let line = self.editor.current_line().to_vec();
        self.editor.checkpoint();
        if self.editor.delete_line().is_some() {
            self.register = Some(Register::Line(line));
        }
    }

    fn delete_word(&mut self) {
        if self.editor.bytes_to_word_forward().is_none() {
            return;
        }
        self.editor.checkpoint();
        if let Some(bytes) = self.editor.delete_to_word_forward() {
            self.register = Some(Register::Character(bytes));
        }
    }

    fn delete_line_end(&mut self) {
        if self.editor.bytes_to_line_end().is_none() {
            return;
        }
        self.editor.checkpoint();
        if let Some(bytes) = self.editor.delete_to_line_end() {
            self.register = Some(Register::Character(bytes));
        }
    }

    fn yank_line(&mut self) {
        self.register = Some(Register::Line(self.editor.current_line().to_vec()));
        self.message.push_str("1 line yanked");
    }

    fn yank_word(&mut self) {
        if let Some(bytes) = self.editor.bytes_to_word_forward() {
            self.register = Some(Register::Character(bytes));
            self.message.push_str("Text yanked");
        }
    }

    fn yank_line_end(&mut self) {
        if let Some(bytes) = self.editor.bytes_to_line_end() {
            self.register = Some(Register::Character(bytes));
            self.message.push_str("Text yanked");
        }
    }

    fn put_register(&mut self, before: bool) -> Result<()> {
        let Some(register) = self.register.clone() else {
            self.message.push_str("Register is empty");
            return Ok(());
        };
        self.editor.checkpoint();
        match register {
            Register::Character(bytes) if before => {
                self.editor.put_before(&bytes)?;
            }
            Register::Character(bytes) => {
                self.editor.put_after(&bytes)?;
            }
            Register::Line(bytes) if before => self.editor.put_line_above(&bytes)?,
            Register::Line(bytes) => self.editor.put_line_below(&bytes)?,
        }
        self.editor.normalize_normal_cursor();
        Ok(())
    }

    fn repeat_search(&mut self, backward: bool) {
        let Some(pattern) = self.last_search.as_ref() else {
            self.message.push_str("No previous search pattern");
            return;
        };
        let found = if backward {
            self.editor.search_backward(pattern)
        } else {
            self.editor.search_forward(pattern)
        };
        if !found {
            self.message
                .push_str(&format!("Pattern not found: {}", pattern.source()));
        }
    }

    fn execute_substitute(&mut self, range: SubstituteRange, expression: &str) {
        let parsed = match parse_substitute_expression(expression) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.message
                    .push_str(&format!("Substitute failed: {error:#}"));
                return;
            }
        };
        let pattern = match RegexPattern::compile(&parsed.pattern) {
            Ok(pattern) => pattern,
            Err(error) => {
                self.message
                    .push_str(&format!("Substitute failed: {error:#}"));
                return;
            }
        };
        match self
            .editor
            .substitute(range, &pattern, &parsed.replacement, parsed.options)
        {
            Ok(result) if result.matches == 0 => self
                .message
                .push_str(&format!("Pattern not found: {}", parsed.pattern)),
            Ok(result) if parsed.options.count_only => self.message.push_str(&format!(
                "{} match(es) on {} line(s)",
                result.matches, result.lines
            )),
            Ok(result) => self.message.push_str(&format!(
                "{} substitution(s) on {} line(s)",
                result.matches, result.lines
            )),
            Err(error) => self
                .message
                .push_str(&format!("Substitute failed: {error:#}")),
        }
    }

    fn move_page(&mut self, down: bool, rows: usize) {
        for _ in 0..rows {
            let moved = if down {
                self.editor.move_down(false)
            } else {
                self.editor.move_up(false)
            };
            if !moved {
                break;
            }
        }
    }

    fn begin_insert_change(&mut self) {
        // One insert session is one undo unit, regardless of how many codepoints
        // are typed before returning to normal mode.
        if !self.insert_changed {
            self.editor.checkpoint();
            self.insert_changed = true;
        }
    }

    fn save(&mut self, force: bool) -> bool {
        let result = if force {
            self.editor.save_force()
        } else {
            self.editor.save()
        };
        match result {
            Ok(()) => {
                self.message.push_str(&format!(
                    "{} written ({} bytes)",
                    self.editor.document().path().display(),
                    self.editor.document().len()
                ));
                true
            }
            Err(error) => {
                self.message.push_str(&format!("Write failed: {error:#}"));
                false
            }
        }
    }

    fn save_all(&mut self, force: bool) -> bool {
        let result = if force {
            self.editor.save_all_force()
        } else {
            self.editor.save_all()
        };
        match result {
            Ok(written) => {
                self.message.push_str(&format!("{written} buffers written"));
                true
            }
            Err(error) => {
                self.message.push_str(&format!("Write failed: {error:#}"));
                false
            }
        }
    }

    fn scroll_window(&mut self, window_id: WindowId, text_rows: usize, columns: usize) {
        let (row, _) = self.editor.position();
        if row < self.window_viewport(window_id).row_offset {
            self.window_viewport_mut(window_id).row_offset = row;
        } else if row >= self.window_viewport(window_id).row_offset + text_rows {
            self.window_viewport_mut(window_id).row_offset = row - text_rows + 1;
        }

        let column = self.cursor_display_column();
        if column < self.window_viewport(window_id).column_offset {
            self.window_viewport_mut(window_id).column_offset = column;
        } else if column >= self.window_viewport(window_id).column_offset + columns {
            self.window_viewport_mut(window_id).column_offset = column - columns + 1;
        }
    }

    fn viewport(&self) -> &Viewport {
        self.window_viewport(self.active_window)
    }

    fn window_viewport(&self, window_id: WindowId) -> &Viewport {
        self.windows
            .get(&window_id)
            .expect("layout references a missing window")
            .viewports
            .get(&self.editor.buffer_id())
            .expect("editor view is missing its viewport")
    }

    fn window_viewport_mut(&mut self, window_id: WindowId) -> &mut Viewport {
        let buffer_id = self.editor.buffer_id();
        self.windows
            .get_mut(&window_id)
            .expect("layout references a missing window")
            .viewports
            .get_mut(&buffer_id)
            .expect("editor view is missing its viewport")
    }

    fn active_window(&self) -> &Window {
        self.windows
            .get(&self.active_window)
            .expect("layout references a missing active window")
    }

    fn active_window_mut(&mut self) -> &mut Window {
        self.windows
            .get_mut(&self.active_window)
            .expect("layout references a missing active window")
    }

    fn cursor_display_column(&self) -> usize {
        let text = String::from_utf8_lossy(self.editor.current_line_prefix());
        display_width(&text)
    }

    fn status_line(&self, columns: usize, active: bool) -> String {
        let mode = match (active, self.mode) {
            (true, Mode::Normal) => "NORMAL",
            (true, Mode::Insert) => "INSERT",
            (true, Mode::Command) => "COMMAND",
            (true, Mode::Search) => "SEARCH",
            (true, Mode::Tree) => "TREE",
            (false, _) => "",
        };
        let dirty = if self.editor.document().is_dirty() {
            " [+]"
        } else {
            ""
        };
        let left = format!(
            " {mode}  [{}/{}] {}{dirty}",
            self.editor.buffer_number(),
            self.editor.buffer_count(),
            self.editor.document().path().display(),
        );
        let (row, column) = self.editor.position();
        let right = format!(" {}:{} ", row + 1, column + 1);
        if right.len() >= columns {
            return render_text(
                right.as_bytes(),
                right.len().saturating_sub(columns),
                columns,
            );
        }
        let available = columns - right.len();
        let mut status = render_text(left.as_bytes(), 0, available);
        let used = display_width(&status);
        status.extend(std::iter::repeat_n(' ', available.saturating_sub(used)));
        status.push_str(&right);
        status
    }
}

fn parse_command(input: &str) -> Command<'_> {
    if let Some(command) = parse_substitute_command(input) {
        return command;
    }
    let mut fields = input.splitn(2, char::is_whitespace);
    let name = fields.next().unwrap_or_default();
    let argument = fields
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match (name, argument) {
        ("", None) => Command::Empty,
        ("w", None) => Command::Write { force: false },
        ("w!", None) => Command::Write { force: true },
        ("wa" | "wall", None) => Command::WriteAll { force: false },
        ("wa!" | "wall!", None) => Command::WriteAll { force: true },
        ("q" | "qa" | "qall", None) => Command::Quit { force: false },
        ("q!" | "qa!" | "qall!", None) => Command::Quit { force: true },
        ("wq" | "x", None) => Command::WriteQuit { force: false },
        ("wq!" | "x!", None) => Command::WriteQuit { force: true },
        ("wqa" | "wqall" | "xa" | "xall", None) => Command::WriteQuitAll { force: false },
        ("wqa!" | "wqall!" | "xa!" | "xall!", None) => Command::WriteQuitAll { force: true },
        ("bn" | "bnext", None) => Command::NextBuffer,
        ("bp" | "bprevious", None) => Command::PreviousBuffer,
        ("b" | "buffer", argument) => Command::Buffer(argument),
        ("bd" | "bdelete", number) => Command::DeleteBuffer {
            force: false,
            number,
        },
        ("bd!" | "bdelete!", number) => Command::DeleteBuffer {
            force: true,
            number,
        },
        ("e" | "edit", argument) => Command::Edit(argument),
        ("buffers" | "ls", None) => Command::ListBuffers,
        ("split" | "sp", path) => Command::Split(SplitAxis::Rows, path),
        ("vsplit" | "vs", path) => Command::Split(SplitAxis::Columns, path),
        ("close" | "clo", None) => Command::CloseWindow,
        ("only", None) => Command::OnlyWindow,
        ("wincmd", command) => Command::Wincmd(command),
        ("resize" | "res", value) => Command::Resize(SplitAxis::Rows, value),
        ("vertical" | "vert", Some(command)) => {
            let mut fields = command.splitn(2, char::is_whitespace);
            let nested = fields.next().unwrap_or_default();
            let value = fields
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if matches!(nested, "resize" | "res") {
                Command::Resize(SplitAxis::Columns, value)
            } else {
                Command::Unknown(input)
            }
        }
        ("tabnew", path) => Command::NewTab(path),
        ("tabclone", None) => Command::CloneTab,
        ("tabrename", title) => Command::RenameTab(title),
        ("tabnext" | "tabn", number) => Command::NextTab(number),
        ("tabprevious" | "tabp", None) => Command::PreviousTab,
        ("tabmove" | "tabm", position) => Command::MoveTab(position),
        ("tabclose" | "tabc", None) => Command::CloseTab,
        ("tabonly" | "tabo", None) => Command::OnlyTab,
        ("tree", root) => Command::Tree(root),
        ("treewidth", width) => Command::TreeWidth(width),
        ("treerefresh", None) => Command::RefreshTree,
        _ => Command::Unknown(input),
    }
}

fn parse_substitute_command(input: &str) -> Option<Command<'_>> {
    for (prefix, range) in [
        ("%s", SubstituteRange::Buffer),
        ("s", SubstituteRange::CurrentLine),
    ] {
        let Some(expression) = input.strip_prefix(prefix) else {
            continue;
        };
        if expression.is_empty() || valid_substitute_delimiter(expression) {
            return Some(Command::Substitute { range, expression });
        }
    }
    None
}

fn valid_substitute_delimiter(expression: &str) -> bool {
    expression.chars().next().is_some_and(|delimiter| {
        delimiter.is_ascii()
            && !delimiter.is_ascii_alphanumeric()
            && !delimiter.is_ascii_whitespace()
            && !matches!(delimiter, '\\' | '"' | '|')
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedSubstitute {
    pattern: String,
    replacement: String,
    options: SubstituteOptions,
}

fn parse_substitute_expression(expression: &str) -> Result<ParsedSubstitute> {
    let delimiter = expression
        .chars()
        .next()
        .filter(|_| valid_substitute_delimiter(expression))
        .context("substitute expression requires a non-alphanumeric delimiter")?;
    let body = &expression[delimiter.len_utf8()..];
    let (pattern, remainder, pattern_closed) = take_substitute_field(body, delimiter);
    anyhow::ensure!(
        pattern_closed,
        "substitute pattern requires a closing delimiter"
    );
    anyhow::ensure!(!pattern.is_empty(), "substitute pattern cannot be empty");
    let (replacement, flags, replacement_closed) = take_substitute_field(remainder, delimiter);
    let flags = if replacement_closed { flags } else { "" };

    let mut options = SubstituteOptions::default();
    for flag in flags.chars() {
        match flag {
            'g' if !options.global => options.global = true,
            'n' if !options.count_only => options.count_only = true,
            'g' | 'n' => anyhow::bail!("duplicate substitute flag {flag:?}"),
            _ => anyhow::bail!("unsupported substitute flag {flag:?}"),
        }
    }
    Ok(ParsedSubstitute {
        pattern,
        replacement,
        options,
    })
}

fn take_substitute_field(input: &str, delimiter: char) -> (String, &str, bool) {
    let mut output = String::with_capacity(input.len());
    for (offset, character) in input.char_indices() {
        if character == delimiter {
            let escaping_backslashes = output
                .chars()
                .rev()
                .take_while(|&char| char == '\\')
                .count();
            if escaping_backslashes % 2 == 1 {
                output.pop();
                output.push(delimiter);
                continue;
            }
            return (output, &input[offset + character.len_utf8()..], true);
        }
        output.push(character);
    }
    (output, "", false)
}

fn parse_resize_amount(value: &str) -> Option<ResizeAmount> {
    if let Some(value) = value.strip_prefix('+') {
        value.parse().ok().map(ResizeAmount::Increase)
    } else if let Some(value) = value.strip_prefix('-') {
        value.parse().ok().map(ResizeAmount::Decrease)
    } else {
        value.parse().ok().map(ResizeAmount::Exact)
    }
}

fn window_direction(key: &Key) -> Option<Direction> {
    match key {
        Key::Char('h') | Key::ArrowLeft | Key::Modified(SpecialKey::ArrowLeft, _) => {
            Some(Direction::Left)
        }
        Key::Char('j') | Key::ArrowDown | Key::Modified(SpecialKey::ArrowDown, _) => {
            Some(Direction::Down)
        }
        Key::Char('k') | Key::ArrowUp | Key::Modified(SpecialKey::ArrowUp, _) => {
            Some(Direction::Up)
        }
        Key::Char('l') | Key::ArrowRight | Key::Modified(SpecialKey::ArrowRight, _) => {
            Some(Direction::Right)
        }
        _ => None,
    }
}

fn move_to(output: &mut Vec<u8>, row: usize, column: usize) {
    output.extend_from_slice(format!("\x1b[{row};{column}H").as_bytes());
}

fn visible_file_tree_width(columns: usize, preferred: usize) -> usize {
    maximum_file_tree_width(columns)
        .map(|maximum| preferred.clamp(MIN_FILE_TREE_WIDTH, maximum))
        .unwrap_or(0)
}

fn maximum_file_tree_width(columns: usize) -> Option<usize> {
    if columns < FILE_TREE_HIDE_COLUMNS {
        return None;
    }
    let maximum = columns.saturating_sub(MIN_EDITOR_WIDTH + 1);
    (maximum >= MIN_FILE_TREE_WIDTH).then_some(maximum)
}

fn document_label(document: &Document) -> String {
    document
        .path()
        .file_name()
        .unwrap_or_else(|| document.path().as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn render_text(bytes: &[u8], column_offset: usize, width: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::new();
    let mut source_column = 0;
    let end = column_offset.saturating_add(width);

    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme_display_width(grapheme, source_column);
        let grapheme_end = source_column + grapheme_width;
        if grapheme_end > column_offset && source_column < end {
            let visible_start = source_column.max(column_offset);
            let visible_end = grapheme_end.min(end);
            if grapheme == "\t" {
                output.extend(std::iter::repeat_n(' ', visible_end - visible_start));
            } else if grapheme.chars().any(char::is_control) {
                output.push('�');
            } else if source_column >= column_offset && grapheme_end <= end {
                output.push_str(grapheme);
            } else {
                // Never emit half of a wide grapheme at a viewport edge. Spaces
                // preserve the occupied cells without corrupting the sequence.
                output.extend(std::iter::repeat_n(' ', visible_end - visible_start));
            }
        }
        source_column = grapheme_end;
        if source_column >= end {
            break;
        }
    }
    output
}

fn display_width(text: &str) -> usize {
    text.graphemes(true).fold(0, |column, grapheme| {
        column + grapheme_display_width(grapheme, column)
    })
}

fn grapheme_display_width(grapheme: &str, column: usize) -> usize {
    if grapheme == "\t" {
        return TAB_STOP - column % TAB_STOP;
    }
    if grapheme.chars().any(char::is_control) {
        return 1;
    }
    UnicodeWidthStr::width(grapheme)
}

fn line_number_width(line_count: usize, columns: usize) -> usize {
    let desired = decimal_width(line_count) + 1;
    // A gutter that consumes the whole viewport is less useful than text, so
    // line numbers disappear only on extremely narrow terminals.
    if desired < columns { desired } else { 0 }
}

fn decimal_width(mut number: usize) -> usize {
    let mut width = 1;
    while number >= 10 {
        number /= 10;
        width += 1;
    }
    width
}

fn render_line_number(row: usize, exists: bool, width: usize) -> String {
    let number_width = width.saturating_sub(1);
    if exists {
        format!("{:>number_width$} ", row + 1)
    } else {
        format!("{:>number_width$} ", "~")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, Command, DEFAULT_FILE_TREE_WIDTH, Mode, ParsedSubstitute, SplitAxis, display_width,
        parse_command, parse_substitute_expression,
    };
    use bed_core::{Document, Editor, SubstituteOptions, SubstituteRange};
    use bed_terminal::{Key, TerminalSize};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEST_FILE: AtomicUsize = AtomicUsize::new(0);

    fn app_with(bytes: &[u8]) -> App {
        App::new(Editor::new(Document::new(
            PathBuf::from("test.txt"),
            bytes.to_vec(),
        )))
    }

    fn execute(app: &mut App, command: &str) {
        app.handle_key(Key::Char(':')).unwrap();
        for character in command.chars() {
            app.handle_key(Key::Char(character)).unwrap();
        }
        app.handle_key(Key::Enter).unwrap();
    }

    #[test]
    fn inserts_text_and_returns_to_normal_mode() {
        let mut app = app_with(b"ac");

        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Char('b')).unwrap();
        app.handle_key(Key::Escape).unwrap();

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.editor().document().as_bytes(), b"bac");
        assert_eq!(app.editor().cursor().offset(), 0);
    }

    #[test]
    fn supports_delete_line_and_undo() {
        let mut app = app_with(b"one\ntwo");

        app.handle_key(Key::Char('d')).unwrap();
        app.handle_key(Key::Char('d')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"two");
        app.handle_key(Key::Char('u')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one\ntwo");
    }

    #[test]
    fn unchanged_insert_mode_does_not_create_an_undo_step() {
        let mut app = app_with(b"a");
        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Char('b')).unwrap();
        app.handle_key(Key::Escape).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"ba");

        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Escape).unwrap();
        app.handle_key(Key::Char('u')).unwrap();

        assert_eq!(app.editor().document().as_bytes(), b"a");
    }

    #[test]
    fn append_without_an_edit_restores_the_normal_cursor() {
        let mut app = app_with(b"ab");

        app.handle_key(Key::Char('a')).unwrap();
        app.handle_key(Key::Escape).unwrap();

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.editor().cursor().offset(), 0);
    }

    #[test]
    fn insert_navigation_cannot_leave_a_normal_cursor_after_text() {
        let mut app = app_with(b"ab");

        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::ArrowRight).unwrap();
        app.handle_key(Key::ArrowRight).unwrap();
        app.handle_key(Key::Escape).unwrap();

        assert_eq!(app.editor().cursor().offset(), 1);
    }

    #[test]
    fn undo_normalizes_the_saved_insert_cursor() {
        let mut app = app_with(b"ab");

        app.handle_key(Key::Char('A')).unwrap();
        app.handle_key(Key::Char('c')).unwrap();
        app.handle_key(Key::Escape).unwrap();
        app.handle_key(Key::Char('u')).unwrap();

        assert_eq!(app.editor().document().as_bytes(), b"ab");
        assert_eq!(app.editor().cursor().offset(), 1);
    }

    #[test]
    fn inserts_tabs_and_platform_line_endings() {
        let mut app = app_with(b"");

        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Tab).unwrap();
        app.handle_key(Key::Enter).unwrap();

        let expected = if cfg!(windows) {
            b"\t\r\n".as_slice()
        } else {
            b"\t\n".as_slice()
        };
        assert_eq!(app.editor().document().as_bytes(), expected);
    }

    #[test]
    fn bracketed_paste_is_one_insert_change() {
        let mut app = app_with(b"start");

        app.handle_key(Key::Char('A')).unwrap();
        app.handle_key(Key::Paste("\n你好\tend".to_owned()))
            .unwrap();
        app.handle_key(Key::Escape).unwrap();
        let expected = if cfg!(windows) {
            "start\r\n你好\tend".as_bytes()
        } else {
            "start\n你好\tend".as_bytes()
        };
        assert_eq!(app.editor().document().as_bytes(), expected);

        app.handle_key(Key::Char('u')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"start");
    }

    #[test]
    fn bracketed_paste_is_not_executed_in_normal_mode() {
        let mut app = app_with(b"text");

        app.handle_key(Key::Paste("dd:q!".to_owned())).unwrap();

        assert_eq!(app.editor().document().as_bytes(), b"text");
        assert!(!app.should_quit());
    }

    #[test]
    fn save_failure_keeps_the_dirty_buffer_open() {
        let path = std::env::temp_dir()
            .join(format!("bed-missing-{}", std::process::id()))
            .join("file.txt");
        let mut editor = Editor::new(Document::new(path, Vec::new()));
        editor.insert_bytes(b"changed").unwrap();
        let mut app = App::new(editor);

        for key in [Key::Char(':'), Key::Char('w'), Key::Char('q'), Key::Enter] {
            app.handle_key(key).unwrap();
        }

        assert!(!app.should_quit());
        assert!(app.editor().document().is_dirty());
        assert!(app.message.starts_with("Write failed:"));
    }

    #[test]
    fn refuses_to_quit_with_unsaved_changes() {
        let mut app = app_with(b"");
        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Char('x')).unwrap();
        app.handle_key(Key::Escape).unwrap();
        app.handle_key(Key::Char(':')).unwrap();
        app.handle_key(Key::Char('q')).unwrap();
        app.handle_key(Key::Enter).unwrap();

        assert!(!app.should_quit());
    }

    #[test]
    fn force_quit_exits() {
        let mut app = app_with(b"");
        for key in [Key::Char(':'), Key::Char('q'), Key::Char('!'), Key::Enter] {
            app.handle_key(key).unwrap();
        }
        assert!(app.should_quit());
    }

    #[test]
    fn switches_buffers_and_restores_their_viewports() {
        let mut editor = Editor::new(Document::new(
            PathBuf::from("one.txt"),
            b"0\n1\n2\n3\n4\n5".to_vec(),
        ));
        editor.add_document(Document::new(PathBuf::from("two.txt"), b"two".to_vec()));
        let mut app = App::new(editor);
        let size = TerminalSize {
            rows: 5,
            columns: 20,
        };

        app.handle_key(Key::Char('G')).unwrap();
        app.render(size);
        assert_eq!(app.viewport().row_offset, 4);

        execute(&mut app, "bnext");
        assert_eq!(app.editor().buffer_number(), 2);
        assert_eq!(app.viewport().row_offset, 0);

        execute(&mut app, "bprevious");
        assert_eq!(app.editor().buffer_number(), 1);
        assert_eq!(app.editor().position(), (5, 0));
        assert_eq!(app.viewport().row_offset, 4);
    }

    #[test]
    fn splits_share_a_buffer_with_independent_cursors() {
        let mut app = app_with(b"abcd");
        execute(&mut app, "vsplit");

        assert_eq!(app.windows.len(), 2);
        let right_view = app.editor().view_id();
        app.handle_key(Key::Char('l')).unwrap();
        assert_eq!(app.editor().cursor().offset(), 1);

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('h')).unwrap();
        assert_eq!(app.editor().cursor().offset(), 0);
        assert_ne!(app.editor().view_id(), right_view);

        let frame = String::from_utf8(app.render(TerminalSize {
            rows: 8,
            columns: 80,
        }))
        .unwrap();
        assert!(frame.contains('│'));
        assert!(frame.matches("test.txt").count() >= 2);

        execute(&mut app, "close");
        assert_eq!(app.windows.len(), 1);
    }

    #[test]
    fn resizes_windows_with_counts_exact_sizes_and_commands() {
        let mut app = app_with(b"");
        let size = TerminalSize {
            rows: 12,
            columns: 80,
        };
        app.render(size);
        execute(&mut app, "vsplit");

        for key in [
            Key::Char('1'),
            Key::Char('0'),
            Key::Ctrl('w'),
            Key::Char('<'),
        ] {
            app.handle_key(key).unwrap();
        }
        let rectangles = app.layout.rectangles(app.editor_area());
        assert_eq!(rectangles[0].1.columns, 39);
        assert_eq!(rectangles[1].1.columns, 19);

        for key in [
            Key::Ctrl('w'),
            Key::Char('1'),
            Key::Char('5'),
            Key::Char('|'),
        ] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.layout.rectangles(app.editor_area())[1].1.columns, 15);

        execute(&mut app, "vertical resize +4");
        assert_eq!(app.layout.rectangles(app.editor_area())[1].1.columns, 19);
        execute(&mut app, "vertical resize 12");
        assert_eq!(app.layout.rectangles(app.editor_area())[1].1.columns, 12);

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('=')).unwrap();
        let rectangles = app.layout.rectangles(app.editor_area());
        assert_eq!(rectangles[0].1.columns, 29);
        assert_eq!(rectangles[1].1.columns, 29);

        execute(&mut app, "split");
        for key in [Key::Ctrl('w'), Key::Char('3'), Key::Char('_')] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.layout.rectangles(app.editor_area())[2].1.rows, 3);
        execute(&mut app, "resize +2");
        assert_eq!(app.layout.rectangles(app.editor_area())[2].1.rows, 5);
    }

    #[test]
    fn resizes_the_focused_file_tree() {
        let mut app = app_with(b"");
        let size = TerminalSize {
            rows: 12,
            columns: 80,
        };
        app.render(size);
        app.handle_key(Key::Ctrl('n')).unwrap();

        for key in [Key::Char('5'), Key::Ctrl('w'), Key::Char('<')] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.file_tree_width(), 15);

        for key in [
            Key::Ctrl('w'),
            Key::Char('1'),
            Key::Char('0'),
            Key::Char('|'),
        ] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.file_tree_width(), 10);

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('=')).unwrap();
        assert_eq!(app.file_tree_width(), DEFAULT_FILE_TREE_WIDTH);

        app.handle_key(Key::Ctrl('n')).unwrap();
        execute(&mut app, "treewidth 15");
        assert_eq!(app.file_tree_width(), 15);
    }

    #[test]
    fn window_navigation_returns_from_the_file_tree_to_the_active_editor() {
        let mut app = app_with(b"");
        app.render(TerminalSize {
            rows: 12,
            columns: 80,
        });
        execute(&mut app, "vsplit");
        let right = app.active_window;

        app.handle_key(Key::Ctrl('n')).unwrap();
        assert_eq!(app.mode(), Mode::Tree);
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('l')).unwrap();
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.active_window, right);

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('h')).unwrap();
        let left = app.active_window;
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('h')).unwrap();
        assert_eq!(app.mode(), Mode::Tree);
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('w')).unwrap();
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.active_window, left);
    }

    #[test]
    fn treewidth_is_remembered_while_the_sidebar_is_hidden() {
        let mut app = app_with(b"");
        execute(&mut app, "treewidth 15");
        assert_eq!(app.file_tree_width, 15);
        assert_eq!(app.file_tree_width(), 0);

        app.render(TerminalSize {
            rows: 12,
            columns: 80,
        });
        assert_eq!(app.file_tree_width(), 15);

        execute(&mut app, "treewidth 9");
        assert_eq!(app.file_tree_width(), 15);
        assert!(app.message.contains("at least 10"));
    }

    #[test]
    fn counts_repeat_normal_mode_motions() {
        let mut app = app_with(b"abcdef\nsecond\nthird");
        for key in [Key::Char('4'), Key::Char('l')] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.editor().position(), (0, 4));

        for key in [Key::Char('2'), Key::Char('j')] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.editor().position(), (2, 4));
    }

    #[test]
    fn each_split_selects_buffers_independently() {
        let mut editor = Editor::new(Document::new(PathBuf::from("one.txt"), b"one".to_vec()));
        editor.add_document(Document::new(PathBuf::from("two.txt"), b"two".to_vec()));
        let mut app = App::new(editor);
        execute(&mut app, "vsplit");
        execute(&mut app, "bnext");
        assert_eq!(app.editor().document().path(), PathBuf::from("two.txt"));

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('h')).unwrap();
        assert_eq!(app.editor().document().path(), PathBuf::from("one.txt"));

        execute(&mut app, "bdelete 2");
        assert_eq!(app.editor().buffer_count(), 1);
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('l')).unwrap();
        assert_eq!(app.editor().document().path(), PathBuf::from("one.txt"));
        app.render(TerminalSize {
            rows: 8,
            columns: 40,
        });
    }

    #[test]
    fn only_closes_every_inactive_window() {
        let mut app = app_with(b"");
        execute(&mut app, "split");
        execute(&mut app, "vsplit");
        assert_eq!(app.windows.len(), 3);

        execute(&mut app, "only");
        assert_eq!(app.windows.len(), 1);
        assert_eq!(app.layout.windows(), vec![app.active_window]);
    }

    #[test]
    fn tab_pages_restore_their_views_and_layouts() {
        let mut app = app_with(b"abcd");
        app.handle_key(Key::Char('l')).unwrap();
        execute(&mut app, "split");
        assert_eq!(app.layout.windows().len(), 2);

        execute(&mut app, "tabnew");
        assert_eq!(app.parked_tabs.len(), 2);
        assert_eq!(app.layout.windows().len(), 1);
        app.handle_key(Key::Char('l')).unwrap();
        assert_eq!(app.editor().cursor().offset(), 2);

        app.handle_key(Key::Char('g')).unwrap();
        app.handle_key(Key::Char('T')).unwrap();
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.layout.windows().len(), 2);
        assert_eq!(app.editor().cursor().offset(), 1);

        app.handle_key(Key::Char('g')).unwrap();
        app.handle_key(Key::Char('t')).unwrap();
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.editor().cursor().offset(), 2);
    }

    #[test]
    fn tab_titles_are_stable_and_can_be_renamed() {
        let mut editor = Editor::new(Document::new(PathBuf::from("one.txt"), b"one".to_vec()));
        editor.add_document(Document::new(PathBuf::from("two.txt"), b"two".to_vec()));
        let mut app = App::new(editor);
        execute(&mut app, "tabnew");
        execute(&mut app, "buffer 2");

        let size = TerminalSize {
            rows: 8,
            columns: 80,
        };
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.contains("1 one.txt"));
        assert!(frame.contains("[2 one.txt]"));
        assert!(!frame.contains("[2 two.txt]"));
        assert!(frame.contains("  1 one.txt\x1b[7m [2 one.txt]\x1b[m"));

        execute(&mut app, "tabrename tests");
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.contains("[2 tests]"));

        execute(&mut app, "tabrename");
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.contains("[2 one.txt]"));
    }

    #[test]
    fn new_tabs_are_inserted_after_the_active_tab() {
        let mut app = app_with(b"");
        execute(&mut app, "tabrename first");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename third");
        for key in [Key::Char('1'), Key::Char('g'), Key::Char('t')] {
            app.handle_key(key).unwrap();
        }

        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename second");

        assert_eq!(app.active_tab, 1);
        assert_eq!(app.parked_tabs.len(), 3);
        assert_eq!(
            app.parked_tabs[0].as_ref().unwrap().title.as_deref(),
            Some("first")
        );
        assert_eq!(app.active_tab_title.as_deref(), Some("second"));
        assert_eq!(
            app.parked_tabs[2].as_ref().unwrap().title.as_deref(),
            Some("third")
        );
    }

    #[test]
    fn cloned_tabs_copy_layout_and_views_but_share_buffers() {
        let mut app = app_with(b"abcd");
        let size = TerminalSize {
            rows: 12,
            columns: 80,
        };
        app.render(size);
        execute(&mut app, "vsplit");
        execute(&mut app, "vertical resize 15");
        app.handle_key(Key::Char('l')).unwrap();
        let source_windows = app.layout.windows();
        let source_rectangles: Vec<_> = app
            .layout
            .rectangles(app.editor_area())
            .into_iter()
            .map(|(_, rectangle)| rectangle)
            .collect();

        execute(&mut app, "tabclone");

        let clone_windows = app.layout.windows();
        let clone_rectangles: Vec<_> = app
            .layout
            .rectangles(app.editor_area())
            .into_iter()
            .map(|(_, rectangle)| rectangle)
            .collect();
        assert_eq!(source_rectangles, clone_rectangles);
        assert!(source_windows.iter().all(|id| !clone_windows.contains(id)));
        assert_eq!(app.editor().cursor().offset(), 1);

        app.handle_key(Key::Char('l')).unwrap();
        app.handle_key(Key::Char('x')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"abd");
        execute(&mut app, "tabprevious");
        assert_eq!(app.editor().cursor().offset(), 1);
        assert_eq!(app.editor().document().as_bytes(), b"abd");
    }

    #[test]
    fn tab_navigation_and_movement_use_one_based_positions() {
        let mut app = app_with(b"");
        execute(&mut app, "tabrename one");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename two");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename three");

        for key in [Key::Char('1'), Key::Char('g'), Key::Char('t')] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.active_tab_title.as_deref(), Some("one"));
        execute(&mut app, "tabnext 3");
        assert_eq!(app.active_tab_title.as_deref(), Some("three"));

        execute(&mut app, "tabmove 1");
        assert_eq!(app.active_tab, 0);
        execute(&mut app, "tabmove 3");
        assert_eq!(app.active_tab, 2);
        assert_eq!(
            app.parked_tabs[0].as_ref().unwrap().title.as_deref(),
            Some("one")
        );
        assert_eq!(
            app.parked_tabs[1].as_ref().unwrap().title.as_deref(),
            Some("two")
        );
        assert_eq!(app.active_tab_title.as_deref(), Some("three"));
    }

    #[test]
    fn tab_key_switches_tabs_and_accepts_an_exact_position() {
        let mut app = app_with(b"");
        execute(&mut app, "tabrename one");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename two");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename three");

        app.handle_key(Key::Tab).unwrap();
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.active_tab_title.as_deref(), Some("one"));

        app.handle_key(Key::BackTab).unwrap();
        assert_eq!(app.active_tab, 2);
        assert_eq!(app.active_tab_title.as_deref(), Some("three"));

        for key in [Key::Char('2'), Key::BackTab] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.active_tab_title.as_deref(), Some("one"));

        for key in [Key::Char('2'), Key::Tab] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.active_tab_title.as_deref(), Some("two"));
    }

    #[test]
    fn closing_a_tab_returns_to_the_most_recent_page() {
        let mut app = app_with(b"");
        execute(&mut app, "tabrename one");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename two");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename three");
        for number in ['1', '3', '2'] {
            for key in [Key::Char(number), Key::Char('g'), Key::Char('t')] {
                app.handle_key(key).unwrap();
            }
        }

        execute(&mut app, "tabclose");

        assert_eq!(app.active_tab_title.as_deref(), Some("three"));
    }

    #[test]
    fn closing_a_window_returns_to_the_most_recent_window() {
        let mut app = app_with(b"");
        execute(&mut app, "vsplit");
        let right = app.active_window;
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('h')).unwrap();
        execute(&mut app, "split");
        let lower_left = app.active_window;
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('l')).unwrap();
        assert_eq!(app.active_window, right);
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('h')).unwrap();
        assert_eq!(app.active_window, lower_left);

        execute(&mut app, "close");

        assert_eq!(app.active_window, right);
    }

    #[test]
    fn split_commands_can_open_a_path_without_changing_the_source_window() {
        let mut app = app_with(b"source");
        let path = std::env::temp_dir().join(format!(
            "bed-split-command-{}-missing.txt",
            std::process::id()
        ));

        execute(&mut app, &format!("vsplit {}", path.display()));

        assert_eq!(app.layout.windows().len(), 2);
        assert_eq!(app.editor().document().path(), path.as_path());
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('h')).unwrap();
        assert_eq!(app.editor().document().path(), PathBuf::from("test.txt"));
    }

    #[test]
    fn tabline_marks_dirty_pages_and_keeps_the_active_label_visible() {
        let mut app = app_with(b"");
        execute(&mut app, "tabrename first-with-a-long-name");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabrename second");
        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Char('x')).unwrap();
        app.handle_key(Key::Escape).unwrap();

        let mut output = Vec::new();
        app.render_tabline(&mut output, 14);
        let tabline = String::from_utf8(output).unwrap();

        assert!(tabline.contains("[2 second+]"));
        assert!(tabline.contains("\x1b[7m"));
        assert!(tabline.contains("\x1b[m"));
    }

    #[test]
    fn tabline_is_persistent_and_belongs_to_the_editor_area() {
        let mut app = app_with(b"");
        let size = TerminalSize {
            rows: 8,
            columns: 80,
        };

        let frame = String::from_utf8(app.render(size)).unwrap();
        let area_before = app.editor_area();
        assert_eq!(area_before.row, 1);
        assert_eq!(area_before.column, 21);
        assert!(frame.contains("\x1b[1;22H\x1b[7m [1 test.txt]\x1b[m"));
        assert!(frame.contains("\x1b[1;21H│"));

        execute(&mut app, "tabnew");
        app.render(size);
        assert_eq!(app.editor_area(), area_before);
    }

    #[test]
    fn file_tree_expands_directories_and_opens_selected_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("bed-tree-ui-{}-{nonce}", std::process::id(),));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("directory")).unwrap();
        std::fs::write(root.join("directory").join("nested.txt"), b"nested").unwrap();
        std::fs::write(root.join("file.txt"), b"file").unwrap();
        let mut app = app_with(b"");
        app.render(TerminalSize {
            rows: 10,
            columns: 80,
        });

        execute(&mut app, &format!("tree {}", root.display()));
        assert_eq!(app.mode(), Mode::Tree);
        app.handle_key(Key::Char('j')).unwrap();
        app.handle_key(Key::Char('l')).unwrap();
        assert_eq!(app.file_tree.entries().len(), 4);
        app.handle_key(Key::Char('j')).unwrap();
        app.handle_key(Key::Enter).unwrap();

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(
            app.editor().document().path(),
            root.join("directory").join("nested.txt")
        );
        assert_eq!(app.editor().document().as_bytes(), b"nested");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_tree_navigation_is_tab_local_while_its_width_is_global() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent =
            std::env::temp_dir().join(format!("bed-tab-tree-{}-{nonce}", std::process::id()));
        let first = parent.join("first-root");
        let second = parent.join("second-root");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("one.txt"), b"one").unwrap();
        std::fs::write(second.join("two.txt"), b"two").unwrap();
        std::fs::write(second.join("three.txt"), b"three").unwrap();
        let mut app = app_with(b"");
        app.render(TerminalSize {
            rows: 12,
            columns: 80,
        });

        execute(&mut app, &format!("tree {}", first.display()));
        app.handle_key(Key::Char('j')).unwrap();
        let first_selection = app.file_tree.selected();
        app.handle_key(Key::Escape).unwrap();
        execute(&mut app, "tabnew");
        execute(&mut app, &format!("tree {}", second.display()));
        app.handle_key(Key::Char('j')).unwrap();
        app.handle_key(Key::Char('j')).unwrap();
        let second_selection = app.file_tree.selected();
        app.handle_key(Key::Escape).unwrap();
        execute(&mut app, "treewidth 15");

        execute(&mut app, "tabprevious");
        assert_eq!(app.file_tree.root_label(), "first-root");
        assert_eq!(app.file_tree.selected(), first_selection);
        assert_eq!(app.file_tree_width, 15);

        execute(&mut app, "tabnext");
        assert_eq!(app.file_tree.root_label(), "second-root");
        assert_eq!(app.file_tree.selected(), second_selection);
        assert_eq!(app.file_tree_width, 15);

        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn closes_tabs_and_can_keep_only_the_current_page() {
        let mut app = app_with(b"");
        execute(&mut app, "tabnew");
        execute(&mut app, "tabnew");
        assert_eq!(app.parked_tabs.len(), 3);

        execute(&mut app, "tabclose");
        assert_eq!(app.parked_tabs.len(), 2);
        execute(&mut app, "tabonly");
        assert_eq!(app.parked_tabs.len(), 1);
        assert_eq!(app.windows.len(), 1);
    }

    #[test]
    fn hidden_dirty_buffers_block_normal_quit() {
        let mut editor = Editor::new(Document::new(PathBuf::from("one.txt"), Vec::new()));
        editor.add_document(Document::new(PathBuf::from("two.txt"), Vec::new()));
        let mut app = App::new(editor);

        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Char('x')).unwrap();
        app.handle_key(Key::Escape).unwrap();
        execute(&mut app, "bn");
        execute(&mut app, "q");

        assert!(!app.should_quit());
        assert!(app.message.contains("No write since last change"));
        execute(&mut app, "q!");
        assert!(app.should_quit());
    }

    #[test]
    fn selects_buffers_by_number_and_lists_the_session() {
        let mut editor = Editor::new(Document::new(PathBuf::from("one.txt"), Vec::new()));
        editor.add_document(Document::new(PathBuf::from("two.txt"), Vec::new()));
        let mut app = App::new(editor);

        execute(&mut app, "buffer 2");
        assert_eq!(app.editor().document().path(), PathBuf::from("two.txt"));
        execute(&mut app, "ls");
        assert!(app.message.contains("1:  one.txt"));
        assert!(app.message.contains("2:% two.txt"));

        execute(&mut app, "b 3");
        assert!(app.message.contains("Buffer 3 does not exist"));
    }

    #[test]
    fn deleting_a_dirty_buffer_requires_force() {
        let mut editor = Editor::new(Document::new(PathBuf::from("one.txt"), Vec::new()));
        editor.add_document(Document::new(PathBuf::from("two.txt"), Vec::new()));
        let mut app = App::new(editor);
        app.handle_key(Key::Char('i')).unwrap();
        app.handle_key(Key::Char('x')).unwrap();
        app.handle_key(Key::Escape).unwrap();

        execute(&mut app, "bdelete");
        assert_eq!(app.editor().buffer_count(), 2);
        assert!(app.message.contains(":bdelete!"));

        execute(&mut app, "bdelete!");
        assert_eq!(app.editor().buffer_count(), 1);
        assert_eq!(app.editor().document().path(), PathBuf::from("two.txt"));
    }

    #[test]
    fn writes_all_dirty_buffers() {
        let directory = std::env::temp_dir();
        let first_path = directory.join(format!("bed-wall-{}-one.txt", std::process::id()));
        let second_path = directory.join(format!("bed-wall-{}-two.txt", std::process::id()));
        let mut editor = Editor::new(Document::new(first_path.clone(), Vec::new()));
        editor.insert_bytes(b"one").unwrap();
        let second = editor.add_document(Document::new(second_path.clone(), Vec::new()));
        editor.switch_buffer(second);
        editor.insert_bytes(b"two").unwrap();
        let mut app = App::new(editor);

        execute(&mut app, "wall");

        assert_eq!(app.editor().dirty_buffer_count(), 0);
        assert_eq!(std::fs::read(&first_path).unwrap(), b"one");
        assert_eq!(std::fs::read(&second_path).unwrap(), b"two");
        std::fs::remove_file(first_path).unwrap();
        std::fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn force_write_overrides_an_external_change() {
        let path = std::env::temp_dir().join(format!(
            "bed-force-write-{}-{}.txt",
            std::process::id(),
            NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"original").unwrap();
        let mut editor = Editor::open(path.clone()).unwrap();
        editor.insert_bytes(b" edited").unwrap();
        std::fs::write(&path, b"external").unwrap();
        let mut app = App::new(editor);

        execute(&mut app, "w");
        assert!(app.message.contains("changed on disk"));
        assert_eq!(std::fs::read(&path).unwrap(), b"external");

        execute(&mut app, "w!");
        assert!(app.message.contains("written"));
        assert_eq!(std::fs::read(&path).unwrap(), b" editedoriginal");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn edit_opens_a_new_buffer_and_reuses_an_existing_path() {
        let mut app = app_with(b"");
        let path = std::env::temp_dir().join(format!(
            "bed-edit-command-{}-missing.txt",
            std::process::id()
        ));
        let command = format!("edit {}", path.display());

        execute(&mut app, &command);
        assert_eq!(app.editor().buffer_count(), 2);
        assert_eq!(app.editor().document().path(), path.as_path());
        execute(&mut app, &command);
        assert_eq!(app.editor().buffer_count(), 2);
    }

    #[test]
    fn parses_buffer_commands_without_ambiguous_prefixes() {
        assert_eq!(parse_command("bn"), Command::NextBuffer);
        assert_eq!(parse_command("bprevious"), Command::PreviousBuffer);
        assert_eq!(parse_command("buffer 12"), Command::Buffer(Some("12")));
        assert_eq!(
            parse_command("edit some path"),
            Command::Edit(Some("some path"))
        );
        assert_eq!(parse_command("buffers"), Command::ListBuffers);
        assert_eq!(parse_command("wall"), Command::WriteAll { force: false });
        assert_eq!(parse_command("w!"), Command::Write { force: true });
        assert_eq!(parse_command("wa!"), Command::WriteAll { force: true });
        assert_eq!(
            parse_command("vsplit"),
            Command::Split(SplitAxis::Columns, None)
        );
        assert_eq!(
            parse_command("split other.txt"),
            Command::Split(SplitAxis::Rows, Some("other.txt"))
        );
        assert_eq!(parse_command("wincmd h"), Command::Wincmd(Some("h")));
        assert_eq!(
            parse_command("resize -3"),
            Command::Resize(SplitAxis::Rows, Some("-3"))
        );
        assert_eq!(
            parse_command("vertical resize 15"),
            Command::Resize(SplitAxis::Columns, Some("15"))
        );
        assert_eq!(parse_command("tabnew"), Command::NewTab(None));
        assert_eq!(
            parse_command("tabnew other.txt"),
            Command::NewTab(Some("other.txt"))
        );
        assert_eq!(parse_command("tabclone"), Command::CloneTab);
        assert_eq!(
            parse_command("tabrename tests"),
            Command::RenameTab(Some("tests"))
        );
        assert_eq!(parse_command("tabnext 3"), Command::NextTab(Some("3")));
        assert_eq!(parse_command("tabmove 2"), Command::MoveTab(Some("2")));
        assert_eq!(parse_command("tabp"), Command::PreviousTab);
        assert_eq!(parse_command("tree src"), Command::Tree(Some("src")));
        assert_eq!(
            parse_command("treewidth 15"),
            Command::TreeWidth(Some("15"))
        );
        assert_eq!(parse_command("treerefresh"), Command::RefreshTree);
        assert_eq!(
            parse_command("bdelete! 2"),
            Command::DeleteBuffer {
                force: true,
                number: Some("2")
            }
        );
        assert_eq!(
            parse_command("bnext extra"),
            Command::Unknown("bnext extra")
        );
    }

    #[test]
    fn renders_document_status_and_cursor() {
        let mut app = app_with("one\n你好".as_bytes());
        let frame = app.render(TerminalSize {
            rows: 6,
            columns: 20,
        });
        let frame = String::from_utf8(frame).unwrap();

        assert!(frame.contains("one"));
        assert!(frame.contains("NORMAL"));
        assert!(frame.contains("1 one"));
        assert!(frame.contains("2 你好"));
        assert!(frame.ends_with("\x1b[2;3H\x1b[?25h"));
    }

    #[test]
    fn status_lines_do_not_overflow_narrow_windows() {
        let app = app_with(b"");

        for columns in 1..12 {
            assert!(display_width(&app.status_line(columns, true)) <= columns);
            assert!(display_width(&app.status_line(columns, false)) <= columns);
        }
    }

    #[test]
    fn renders_command_cursor_after_the_prompt() {
        let mut app = app_with(b"");
        let size = TerminalSize {
            rows: 6,
            columns: 20,
        };

        app.handle_key(Key::Char(':')).unwrap();
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.ends_with(":\x1b[6;2H\x1b[?25h"));

        app.handle_key(Key::Char('w')).unwrap();
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.ends_with(":w\x1b[6;3H\x1b[?25h"));
    }

    #[test]
    fn scrolls_long_commands_to_keep_the_cursor_visible() {
        let mut app = app_with(b"");
        app.handle_key(Key::Char(':')).unwrap();
        for character in "write".chars() {
            app.handle_key(Key::Char(character)).unwrap();
        }

        let frame = app.render(TerminalSize {
            rows: 6,
            columns: 4,
        });
        let frame = String::from_utf8(frame).unwrap();

        assert!(frame.ends_with("ite\x1b[6;4H\x1b[?25h"));
    }

    #[test]
    fn renders_complex_emoji_as_one_double_width_grapheme() {
        assert_eq!(display_width("👩🏽‍💻"), 2);
        assert_eq!(display_width("🇨🇳"), 2);

        let mut app = app_with("👩🏽‍💻x".as_bytes());
        app.handle_key(Key::Char('l')).unwrap();
        let frame = app.render(TerminalSize {
            rows: 6,
            columns: 20,
        });
        let frame = String::from_utf8(frame).unwrap();

        assert!(frame.ends_with("\x1b[2;5H\x1b[?25h"));
    }

    #[test]
    fn supports_redo_from_normal_mode() {
        let mut app = app_with(b"a");
        app.handle_key(Key::Char('x')).unwrap();
        app.handle_key(Key::Char('u')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"a");

        app.handle_key(Key::Ctrl('r')).unwrap();

        assert!(app.editor().document().is_empty());
    }

    #[test]
    fn supports_word_motions_and_delete_operators() {
        let mut app = app_with("one 你好, three".as_bytes());

        app.handle_key(Key::Char('w')).unwrap();
        assert_eq!(app.editor().current_line_prefix(), b"one ");
        app.handle_key(Key::Char('d')).unwrap();
        app.handle_key(Key::Char('w')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), "one , three".as_bytes());
        app.handle_key(Key::Char('d')).unwrap();
        app.handle_key(Key::Char('$')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one ");
    }

    #[test]
    fn yanks_deletes_and_puts_lines() {
        let mut app = app_with(b"one\ntwo");

        app.handle_key(Key::Char('y')).unwrap();
        app.handle_key(Key::Char('y')).unwrap();
        app.handle_key(Key::Char('j')).unwrap();
        app.handle_key(Key::Char('p')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one\ntwo\none");

        app.handle_key(Key::Char('d')).unwrap();
        app.handle_key(Key::Char('d')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one\ntwo");
        app.handle_key(Key::Char('p')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one\ntwo\none");
    }

    #[test]
    fn searches_forward_and_repeats_in_both_directions() {
        let mut app = app_with(b"one two one ONe");

        for key in [
            Key::Char('/'),
            Key::Char('('),
            Key::Char('?'),
            Key::Char('i'),
            Key::Char(')'),
            Key::Char('o'),
            Key::Char('.'),
            Key::Char('e'),
            Key::Enter,
        ] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.editor().cursor().offset(), 8);
        app.handle_key(Key::Char('n')).unwrap();
        assert_eq!(app.editor().cursor().offset(), 12);
        app.handle_key(Key::Char('n')).unwrap();
        assert_eq!(app.editor().cursor().offset(), 0);
        app.handle_key(Key::Char('N')).unwrap();
        assert_eq!(app.editor().cursor().offset(), 12);
    }

    #[test]
    fn invalid_search_does_not_replace_the_last_pattern() {
        let mut app = app_with(b"one two one");
        for key in [
            Key::Char('/'),
            Key::Char('o'),
            Key::Char('.'),
            Key::Char('e'),
            Key::Enter,
        ] {
            app.handle_key(key).unwrap();
        }
        assert_eq!(app.editor().cursor().offset(), 8);

        for key in [Key::Char('/'), Key::Char('('), Key::Enter] {
            app.handle_key(key).unwrap();
        }
        assert!(app.message.starts_with("Search failed:"));
        app.handle_key(Key::Char('n')).unwrap();
        assert_eq!(app.editor().cursor().offset(), 0);
    }

    #[test]
    fn substitutes_captures_and_undoes_as_one_change() {
        let mut app = app_with(b"a=1 a=2\nb=3");

        execute(&mut app, "%s/(?P<name>[a-z])=([0-9])/$2:${name}/g");

        assert_eq!(app.editor().document().as_bytes(), b"1:a 2:a\n3:b");
        assert_eq!(app.message, "3 substitution(s) on 2 line(s)");
        app.handle_key(Key::Char('u')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"a=1 a=2\nb=3");
    }

    #[test]
    fn substitute_errors_and_counting_do_not_modify_the_buffer() {
        let mut app = app_with(b"one one\ntwo");

        execute(&mut app, "%s/o.e/x/gn");
        assert_eq!(app.message, "2 match(es) on 1 line(s)");
        assert_eq!(app.editor().document().as_bytes(), b"one one\ntwo");

        execute(&mut app, "%s/(/x/");
        assert!(app.message.starts_with("Substitute failed:"));
        assert_eq!(app.editor().document().as_bytes(), b"one one\ntwo");
    }

    #[test]
    fn parses_bed_substitute_syntax() {
        assert_eq!(
            parse_command("%s#(?i)a\\#b#${name}#gn"),
            Command::Substitute {
                range: SubstituteRange::Buffer,
                expression: "#(?i)a\\#b#${name}#gn",
            }
        );
        assert_eq!(
            parse_substitute_expression("#(?i)a\\#b#${name}#gn").unwrap(),
            ParsedSubstitute {
                pattern: "(?i)a#b".to_owned(),
                replacement: "${name}".to_owned(),
                options: SubstituteOptions {
                    global: true,
                    count_only: true,
                },
            }
        );
        assert!(parse_substitute_expression("/a/b/gg").is_err());
        assert!(parse_substitute_expression("/a/b/i").is_err());
        assert_eq!(
            parse_substitute_expression(r"/\\/slash/").unwrap().pattern,
            r"\\"
        );
    }

    #[test]
    fn page_keys_move_by_the_rendered_viewport() {
        let mut app = app_with(b"0\n1\n2\n3\n4\n5");
        app.render(TerminalSize {
            rows: 6,
            columns: 20,
        });

        app.handle_key(Key::PageDown).unwrap();
        assert_eq!(app.editor().position(), (3, 0));
        app.handle_key(Key::Ctrl('u')).unwrap();
        assert_eq!(app.editor().position(), (2, 0));
        app.handle_key(Key::PageUp).unwrap();
        assert_eq!(app.editor().position(), (0, 0));
    }

    #[test]
    fn renders_search_prompt_and_cursor() {
        let mut app = app_with(b"");
        let size = TerminalSize {
            rows: 6,
            columns: 20,
        };

        app.handle_key(Key::Char('/')).unwrap();
        app.handle_key(Key::Char('x')).unwrap();
        let frame = String::from_utf8(app.render(size)).unwrap();

        assert!(frame.ends_with("/x\x1b[6;3H\x1b[?25h"));
    }
}
