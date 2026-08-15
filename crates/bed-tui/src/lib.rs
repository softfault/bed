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
    BufferId, Document, Editor, RegexPattern, SelectionKind, SubstituteOptions, SubstituteRange,
    ViewId,
};
use bed_terminal::{Key, MouseEvent, SpecialKey, TerminalSize};
use bed_terminal_session::{TerminalSessionId, TerminalStore};
use bed_vt100::{Attributes, Color, Row, Screen};
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
const BLOCK_CURSOR: &[u8] = b"\x1b[2 q";
const BAR_CURSOR: &[u8] = b"\x1b[6 q";
const REVERSE_VIDEO: &[u8] = b"\x1b[7m";
const RESET_STYLE: &[u8] = b"\x1b[m";
const VERTICAL_SEPARATOR: &[u8] = "│".as_bytes();
const TABLINE_ROWS: usize = 1;
const DEFAULT_FILE_TREE_WIDTH: usize = 20;
const MIN_FILE_TREE_WIDTH: usize = 10;
const MIN_EDITOR_WIDTH: usize = 12;
const FILE_TREE_HIDE_COLUMNS: usize = 40;
const TERMINAL_SCROLLBACK_ROWS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    Command,
    Search,
    Tree,
    TerminalInput,
    TerminalNormal,
    TerminalVisual,
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
    Terminal(Option<&'a str>),
    ListTerminals,
    AttachTerminal(Option<&'a str>),
    CloseTerminal {
        force: bool,
        id: Option<&'a str>,
    },
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
    content: WindowContent,
    view_id: ViewId,
    views: HashMap<BufferId, ViewId>,
    viewports: HashMap<BufferId, Viewport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowContent {
    Text,
    Terminal(TerminalViewId),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TerminalViewId(u64);

#[derive(Clone, Copy, Debug)]
struct TerminalView {
    session_id: TerminalSessionId,
    scrollback: usize,
    cursor: TerminalPosition,
    selection: Option<TerminalSelection>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct TerminalPosition {
    row: usize,
    column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalSelection {
    anchor: TerminalPosition,
    cursor: TerminalPosition,
    kind: SelectionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalSelectionBounds {
    start: TerminalPosition,
    end: TerminalPosition,
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
            content: WindowContent::Text,
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
    terminals: TerminalStore,
    terminal_views: HashMap<TerminalViewId, TerminalView>,
    next_terminal_view_id: u64,
    mode: Mode,
    command: String,
    command_selection: Option<SelectionKind>,
    search: String,
    last_search: Option<RegexPattern>,
    message: String,
    pending: Option<Pending>,
    count: Option<usize>,
    register: Option<Register>,
    terminal_prefix: bool,
    command_return_mode: Mode,
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
            terminals: TerminalStore::new(),
            terminal_views: HashMap::new(),
            next_terminal_view_id: 0,
            mode: Mode::Normal,
            command: String::new(),
            command_selection: None,
            search: String::new(),
            last_search: None,
            message: String::from("i: insert  :w: save  :q: quit"),
            pending: None,
            count: None,
            register: None,
            terminal_prefix: false,
            command_return_mode: Mode::Normal,
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

    pub fn handle_key(&mut self, key: Key) -> Result<bool> {
        let had_message = !self.message.is_empty();
        self.message.clear();
        let redraw = match self.mode {
            Mode::Normal => {
                self.handle_normal_key(key)?;
                true
            }
            Mode::Insert => {
                self.handle_insert_key(key)?;
                true
            }
            Mode::Visual => {
                self.handle_visual_key(key, SelectionKind::Character)?;
                true
            }
            Mode::VisualLine => {
                self.handle_visual_key(key, SelectionKind::Line)?;
                true
            }
            Mode::Command => {
                self.handle_command_key(key)?;
                true
            }
            Mode::Search => {
                self.handle_search_key(key);
                true
            }
            Mode::Tree => {
                self.handle_tree_key(key);
                true
            }
            Mode::TerminalInput => self.handle_terminal_input_key(key),
            Mode::TerminalNormal => {
                self.handle_terminal_normal_key(key);
                true
            }
            Mode::TerminalVisual => {
                self.handle_terminal_visual_key(key);
                true
            }
        };
        Ok(redraw || had_message || !self.message.is_empty())
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.mode != Mode::TerminalInput {
            return false;
        }
        let Some(view_id) = self.active_terminal_view_id() else {
            return false;
        };
        let Some((_, area)) = self
            .layout
            .rectangles(self.editor_area())
            .into_iter()
            .find(|(window_id, _)| *window_id == self.active_window)
        else {
            return false;
        };
        let text_rows = area.rows.saturating_sub(1);
        if mouse.row < area.row
            || mouse.row >= area.row.saturating_add(text_rows)
            || mouse.column < area.column
            || mouse.column >= area.column.saturating_add(area.columns)
        {
            return false;
        }
        let Some(view) = self.terminal_views.get(&view_id) else {
            return false;
        };
        if view.scrollback > 0 {
            return false;
        }
        let Some(session) = self.terminals.get(view.session_id) else {
            return false;
        };
        let child_event = MouseEvent {
            row: mouse.row - area.row,
            column: mouse.column - area.column,
            ..mouse
        };
        if let Err(error) = session.send_mouse(child_event) {
            self.message = format!("Terminal mouse failed: {error:#}");
            return true;
        }
        false
    }

    pub fn handle_resize(&mut self, size: TerminalSize) {
        self.last_size = size;
    }

    pub fn render(&mut self, size: TerminalSize) -> Vec<u8> {
        self.last_size = size;
        let rows = size.rows.max(3);
        let columns = size.columns.max(1);
        if self.mode == Mode::Tree && visible_file_tree_width(columns, self.file_tree_width) == 0 {
            self.set_mode_for_active_window();
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
            let (content, view_id) = {
                let window = self
                    .windows
                    .get(&window_id)
                    .expect("layout references a missing window");
                (window.content, window.view_id)
            };
            match content {
                WindowContent::Text => {
                    let switched = self.editor.switch_view(view_id);
                    debug_assert!(switched);
                    if window_id == active_window && self.mode != Mode::Insert {
                        self.editor.normalize_normal_cursor();
                    }
                    self.render_window(&mut output, window_id, area);
                    if window_id == active_window {
                        editor_cursor = self.window_cursor(area);
                    }
                }
                WindowContent::Terminal(terminal_view) => {
                    if window_id == active_window {
                        self.resize_terminal_view(terminal_view, area);
                    }
                    self.render_terminal_window(&mut output, terminal_view, area);
                    if window_id == active_window {
                        editor_cursor = self.terminal_cursor(terminal_view, area);
                    }
                }
            }
        }
        let switched = self.editor.switch_view(active_view);
        debug_assert!(switched);

        let prompt = match self.mode {
            Mode::Command => format!(":{}", self.command),
            Mode::Search => format!("/{}", self.search),
            Mode::Normal
            | Mode::Insert
            | Mode::Visual
            | Mode::VisualLine
            | Mode::Tree
            | Mode::TerminalInput
            | Mode::TerminalNormal
            | Mode::TerminalVisual => self.message.clone(),
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
            Mode::Normal
            | Mode::Insert
            | Mode::Visual
            | Mode::VisualLine
            | Mode::TerminalInput
            | Mode::TerminalNormal
            | Mode::TerminalVisual => editor_cursor,
        };
        move_to(
            &mut output,
            cursor_row.min(rows),
            cursor_column.min(columns),
        );
        output.extend_from_slice(if matches!(self.mode, Mode::Insert | Mode::TerminalInput) {
            BAR_CURSOR
        } else {
            BLOCK_CURSOR
        });
        let cursor_visible = if self.mode == Mode::TerminalInput {
            self.active_terminal_session_id()
                .and_then(|id| self.terminals.get(id))
                .is_some_and(|session| session.screen().cursor().visible)
        } else {
            true
        };
        output.extend_from_slice(if cursor_visible {
            b"\x1b[?25h"
        } else {
            b"\x1b[?25l"
        });
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
        let selection_kind = match self.mode {
            Mode::Visual => Some(SelectionKind::Character),
            Mode::VisualLine => Some(SelectionKind::Line),
            _ => None,
        };
        let selection = (window_id == self.active_window)
            .then(|| {
                selection_kind
                    .and_then(|kind| self.editor.selection_range(kind).map(|range| (range, kind)))
            })
            .flatten();
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
                let line_start = self
                    .editor
                    .line_byte_range(document_row)
                    .expect("rendered line is missing its byte range")
                    .start;
                output.extend_from_slice(
                    render_text_with_selection(
                        line,
                        line_start,
                        selection.as_ref().map(|(range, kind)| (range, *kind)),
                        viewport.column_offset,
                        text_columns,
                    )
                    .as_bytes(),
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

    fn render_terminal_window(&self, output: &mut Vec<u8>, view_id: TerminalViewId, area: Rect) {
        if area.columns == 0 || area.rows == 0 {
            return;
        }
        let view = self
            .terminal_views
            .get(&view_id)
            .expect("terminal window references a missing view");
        let session = self
            .terminals
            .get(view.session_id)
            .expect("terminal view references a missing session");
        let screen = session.screen();
        let text_rows = area.rows.saturating_sub(1);
        let history = screen.scrollback();
        let visible_start = history.len().saturating_sub(view.scrollback);
        let selection = (self.active_terminal_view_id() == Some(view_id)
            && self.mode == Mode::TerminalVisual)
            .then_some(view.selection)
            .flatten()
            .map(|selection| terminal_selection_bounds(screen, selection));
        let rows = history
            .iter()
            .skip(visible_start)
            .chain(screen.rows().iter())
            .take(text_rows);
        for (screen_row, row) in rows.enumerate() {
            move_to(output, area.row + screen_row + 1, area.column + 1);
            render_terminal_row(
                output,
                row,
                area.columns,
                selection.map(|selection| (visible_start + screen_row, selection)),
            );
        }
        output.extend_from_slice(RESET_STYLE);

        move_to(output, area.row + area.rows, area.column + 1);
        output.extend_from_slice(REVERSE_VIDEO);
        let state = if let Some(status) = session.status() {
            format!("exited {status}")
        } else if session.error().is_some() {
            "error".to_owned()
        } else {
            "running".to_owned()
        };
        let name = if session.title().is_empty() {
            session.command()
        } else {
            session.title()
        };
        let mode = if self.active_terminal_view_id() == Some(view_id) {
            match self.mode {
                Mode::TerminalInput => "TERMINAL INPUT",
                Mode::TerminalNormal => "TERMINAL NORMAL",
                Mode::TerminalVisual
                    if view
                        .selection
                        .is_some_and(|selection| selection.kind == SelectionKind::Line) =>
                {
                    "TERMINAL VISUAL LINE"
                }
                Mode::TerminalVisual => "TERMINAL VISUAL",
                _ => "TERMINAL",
            }
        } else {
            "TERMINAL"
        };
        let label = format!(" {mode} {name} [{state}]");
        let mut label = render_text(label.as_bytes(), 0, area.columns);
        let used = display_width(&label);
        label.extend(std::iter::repeat_n(' ', area.columns.saturating_sub(used)));
        output.extend_from_slice(label.as_bytes());
        output.extend_from_slice(RESET_STYLE);

        if area.column > 0 {
            for row in area.row..area.row + area.rows {
                move_to(output, row + 1, area.column);
                output.extend_from_slice(VERTICAL_SEPARATOR);
            }
        }
    }

    fn terminal_cursor(&self, view_id: TerminalViewId, area: Rect) -> (usize, usize) {
        let Some(view) = self.terminal_views.get(&view_id) else {
            return (area.row + 1, area.column + 1);
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return (area.row + 1, area.column + 1);
        };
        let navigation_cursor = match self.mode {
            Mode::TerminalNormal => Some(view.cursor),
            Mode::TerminalVisual => view.selection.map(|selection| selection.cursor),
            _ => None,
        };
        if let Some(cursor) = navigation_cursor {
            let visible_start = session
                .screen()
                .scrollback()
                .len()
                .saturating_sub(view.scrollback);
            let display_column = terminal_row(session.screen(), cursor.row)
                .map_or(cursor.column, |row| {
                    terminal_display_column(row, cursor.column)
                });
            return (
                area.row
                    + cursor
                        .row
                        .saturating_sub(visible_start)
                        .min(area.rows.saturating_sub(2))
                    + 1,
                area.column + display_column.min(area.columns.saturating_sub(1)) + 1,
            );
        }
        let cursor = session.screen().cursor();
        let display_column = session
            .screen()
            .row(cursor.row)
            .map_or(cursor.column, |row| {
                terminal_display_column(row, cursor.column)
            });
        (
            area.row + cursor.row.min(area.rows.saturating_sub(2)) + 1,
            area.column + display_column.min(area.columns.saturating_sub(1)) + 1,
        )
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

    pub fn poll_terminals(&mut self) -> Result<bool> {
        let previous_history: HashMap<_, _> = self
            .terminals
            .ids()
            .filter_map(|id| {
                self.terminals.get(id).map(|session| {
                    (
                        id,
                        (
                            session.history_rows_pushed(),
                            session.history_rows_discarded(),
                            session.screen_generation(),
                        ),
                    )
                })
            })
            .collect();
        let activity = self.terminals.poll()?;
        let active_terminal_view = self.active_terminal_view_id();
        let mut active_selection_cleared = false;
        for (&view_id, view) in &mut self.terminal_views {
            let (before, discarded_before, generation_before) = previous_history
                .get(&view.session_id)
                .copied()
                .unwrap_or((0, 0, 0));
            let (after, discarded_after, generation_after, maximum) =
                self.terminals.get(view.session_id).map_or(
                    (before, discarded_before, generation_before, 0),
                    |session| {
                        (
                            session.history_rows_pushed(),
                            session.history_rows_discarded(),
                            session.screen_generation(),
                            session.scrollback_len(),
                        )
                    },
                );
            if generation_after != generation_before {
                view.scrollback = 0;
                view.cursor = self
                    .terminals
                    .get(view.session_id)
                    .map_or(TerminalPosition::default(), |session| {
                        terminal_live_position(session.screen())
                    });
                active_selection_cleared |=
                    Some(view_id) == active_terminal_view && view.selection.is_some();
                view.selection = None;
                continue;
            }
            if view.scrollback > 0 {
                view.scrollback =
                    anchored_terminal_scrollback(view.scrollback, before, after, maximum);
            }
            let discarded = usize::try_from(discarded_after.saturating_sub(discarded_before))
                .unwrap_or(usize::MAX);
            view.cursor.row = view.cursor.row.saturating_sub(discarded);
            if let Some(selection) = view.selection {
                view.selection = shift_terminal_selection(selection, discarded);
                active_selection_cleared |=
                    Some(view_id) == active_terminal_view && view.selection.is_none();
            }
        }
        if active_selection_cleared && self.mode == Mode::TerminalVisual {
            self.mode = Mode::TerminalNormal;
        }
        for (id, result) in &activity {
            if result.bells > 0 {
                if !self.message.is_empty() {
                    self.message.push_str("; ");
                }
                self.message
                    .push_str(&format!("Terminal {} bell", id.get()));
                if result.bells > 1 {
                    self.message.push_str(&format!(" ({})", result.bells));
                }
            }
            if result.visual_bells > 0 {
                if !self.message.is_empty() {
                    self.message.push_str("; ");
                }
                self.message
                    .push_str(&format!("Terminal {} visual bell", id.get()));
                if result.visual_bells > 1 {
                    self.message
                        .push_str(&format!(" ({})", result.visual_bells));
                }
            }
            if let Some(error) = self.terminals.get(*id).and_then(|session| session.error()) {
                if !self.message.is_empty() {
                    self.message.push_str("; ");
                }
                self.message
                    .push_str(&format!("Terminal {} failed: {error}", id.get()));
            }
        }
        if self.mode == Mode::TerminalInput
            && self
                .active_terminal_session_id()
                .and_then(|id| self.terminals.get(id))
                .is_some_and(|session| session.status().is_some() && session.reached_eof())
        {
            self.enter_terminal_normal();
        }
        Ok(!activity.is_empty())
    }

    fn active_terminal_view_id(&self) -> Option<TerminalViewId> {
        match self.active_window().content {
            WindowContent::Terminal(view_id) => Some(view_id),
            WindowContent::Text => None,
        }
    }

    fn active_terminal_session_id(&self) -> Option<TerminalSessionId> {
        self.active_terminal_view_id()
            .and_then(|view_id| self.terminal_views.get(&view_id))
            .map(|view| view.session_id)
    }

    fn send_active_terminal_key(&mut self, key: &Key) -> bool {
        let result = self
            .active_terminal_session_id()
            .context("active window is not a terminal")
            .and_then(|session_id| {
                self.terminals
                    .get(session_id)
                    .context("active terminal session no longer exists")?
                    .send_key(key)
            });
        if let Err(error) = result {
            self.message
                .push_str(&format!("Terminal input failed: {error:#}"));
            return true;
        }
        false
    }

    fn set_active_terminal_scrollback(&mut self, rows: usize) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let maximum = self
            .terminal_views
            .get(&view_id)
            .and_then(|view| self.terminals.get(view.session_id))
            .map_or(0, |session| session.screen().scrollback().len());
        if let Some(view) = self.terminal_views.get_mut(&view_id) {
            view.scrollback = rows.min(maximum);
        }
    }

    fn sync_active_terminal_cursor_to_child(&mut self) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let cursor = terminal_live_position(session.screen());
        let view = self
            .terminal_views
            .get_mut(&view_id)
            .expect("active terminal view remains present");
        view.scrollback = 0;
        view.cursor = cursor;
        view.selection = None;
    }

    fn enter_terminal_normal(&mut self) {
        self.sync_active_terminal_cursor_to_child();
        self.mode = Mode::TerminalNormal;
        self.terminal_prefix = false;
    }

    fn move_terminal_cursor(&mut self, rows: isize, columns: isize) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let cursor = move_terminal_position(session.screen(), view.cursor, rows, columns);
        self.terminal_views
            .get_mut(&view_id)
            .expect("active terminal view remains present")
            .cursor = cursor;
        self.reveal_terminal_position(view_id, cursor);
    }

    fn move_terminal_cursor_to_edge(&mut self, end: bool) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let mut cursor = normalize_terminal_position(session.screen(), view.cursor);
        cursor.column = if end {
            terminal_row(session.screen(), cursor.row).map_or(0, last_terminal_column)
        } else {
            0
        };
        self.terminal_views
            .get_mut(&view_id)
            .expect("active terminal view remains present")
            .cursor = cursor;
    }

    fn move_terminal_cursor_to_live(&mut self) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let cursor = terminal_live_position(session.screen());
        let view = self
            .terminal_views
            .get_mut(&view_id)
            .expect("active terminal view remains present");
        view.scrollback = 0;
        view.cursor = cursor;
    }

    fn active_terminal_page_rows(&self) -> usize {
        self.layout
            .rectangles(self.editor_area())
            .into_iter()
            .find(|(window_id, _)| *window_id == self.active_window)
            .map_or(1, |(_, area)| area.rows.saturating_sub(1).max(1))
    }

    fn handle_terminal_input_key(&mut self, key: Key) -> bool {
        if self.pending.take() == Some(Pending::Window) {
            self.sync_active_terminal_cursor_to_child();
            self.execute_window_key(&key, None, false);
            return true;
        }
        if self.terminal_prefix {
            self.terminal_prefix = false;
            return match key {
                Key::Ctrl('n') => {
                    self.enter_terminal_normal();
                    true
                }
                Key::Ctrl('w') => {
                    self.pending = Some(Pending::Window);
                    false
                }
                Key::Ctrl('\\') => self.send_active_terminal_key(&Key::Ctrl('\\')),
                _ => {
                    self.message.push_str("Invalid terminal prefix");
                    true
                }
            };
        }
        match key {
            Key::Ctrl('\\') => {
                self.terminal_prefix = true;
                false
            }
            key => self.send_active_terminal_key(&key),
        }
    }

    fn handle_terminal_normal_key(&mut self, key: Key) {
        if self.pending.take() == Some(Pending::Window) {
            self.execute_window_key(&key, None, false);
            return;
        }
        match key {
            Key::Char(':') => self.enter_command_mode(None),
            Key::Ctrl('w') => self.pending = Some(Pending::Window),
            Key::Char('i') | Key::Char('a') | Key::Enter => {
                self.set_active_terminal_scrollback(0);
                self.mode = Mode::TerminalInput;
            }
            Key::Char('h') | Key::ArrowLeft => self.move_terminal_cursor(0, -1),
            Key::Char('l') | Key::ArrowRight => self.move_terminal_cursor(0, 1),
            Key::Char('j') | Key::ArrowDown => self.move_terminal_cursor(1, 0),
            Key::Char('k') | Key::ArrowUp => self.move_terminal_cursor(-1, 0),
            Key::Char('0') | Key::Home => self.move_terminal_cursor_to_edge(false),
            Key::Char('$') => self.move_terminal_cursor_to_edge(true),
            Key::PageUp => {
                self.move_terminal_cursor(-(self.active_terminal_page_rows() as isize), 0);
            }
            Key::PageDown => {
                self.move_terminal_cursor(self.active_terminal_page_rows() as isize, 0);
            }
            Key::Ctrl('u') => {
                self.move_terminal_cursor(
                    -((self.active_terminal_page_rows() / 2).max(1) as isize),
                    0,
                );
            }
            Key::Ctrl('d') => {
                self.move_terminal_cursor(
                    (self.active_terminal_page_rows() / 2).max(1) as isize,
                    0,
                );
            }
            Key::Char('G') | Key::End => self.move_terminal_cursor_to_live(),
            Key::Char('v') => self.enter_terminal_visual(SelectionKind::Character),
            Key::Char('V') => self.enter_terminal_visual(SelectionKind::Line),
            Key::Escape => {}
            _ => self.message.push_str("Invalid Terminal Normal command"),
        }
    }

    fn enter_terminal_visual(&mut self, kind: SelectionKind) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let position = normalize_terminal_position(session.screen(), view.cursor);
        self.terminal_views
            .get_mut(&view_id)
            .expect("active terminal view remains present")
            .selection = Some(TerminalSelection {
            anchor: position,
            cursor: position,
            kind,
        });
        self.mode = Mode::TerminalVisual;
    }

    fn handle_terminal_visual_key(&mut self, key: Key) {
        match key {
            Key::Escape | Key::Ctrl('c') => self.leave_terminal_visual(),
            Key::Char('v') => self.toggle_terminal_selection_kind(SelectionKind::Character),
            Key::Char('V') => self.toggle_terminal_selection_kind(SelectionKind::Line),
            Key::Char('h') | Key::ArrowLeft => self.move_terminal_selection(0, -1),
            Key::Char('l') | Key::ArrowRight => self.move_terminal_selection(0, 1),
            Key::Char('j') | Key::ArrowDown => self.move_terminal_selection(1, 0),
            Key::Char('k') | Key::ArrowUp => self.move_terminal_selection(-1, 0),
            Key::Char('0') | Key::Home => self.move_terminal_selection_to_edge(false),
            Key::Char('$') | Key::End => self.move_terminal_selection_to_edge(true),
            Key::Char('y') => self.yank_terminal_selection(),
            _ => self.message.push_str("Invalid Terminal Visual command"),
        }
    }

    fn toggle_terminal_selection_kind(&mut self, kind: SelectionKind) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(selection) = self
            .terminal_views
            .get(&view_id)
            .and_then(|view| view.selection)
        else {
            return;
        };
        if selection.kind == kind {
            self.leave_terminal_visual();
        } else if let Some(selection) = self
            .terminal_views
            .get_mut(&view_id)
            .and_then(|view| view.selection.as_mut())
        {
            selection.kind = kind;
        }
    }

    fn leave_terminal_visual(&mut self) {
        if let Some(view_id) = self.active_terminal_view_id()
            && let Some(view) = self.terminal_views.get_mut(&view_id)
        {
            if let Some(selection) = view.selection {
                view.cursor = selection.cursor;
            }
            view.selection = None;
        }
        self.mode = Mode::TerminalNormal;
    }

    fn move_terminal_selection(&mut self, rows: isize, columns: isize) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let Some(mut selection) = view.selection else {
            return;
        };
        selection.cursor =
            move_terminal_position(session.screen(), selection.cursor, rows, columns);
        self.terminal_views
            .get_mut(&view_id)
            .expect("active terminal view remains present")
            .selection = Some(selection);
        self.reveal_terminal_position(view_id, selection.cursor);
    }

    fn move_terminal_selection_to_edge(&mut self, end: bool) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let Some(mut selection) = view.selection else {
            return;
        };
        selection.cursor.column = if end {
            terminal_row(session.screen(), selection.cursor.row).map_or(0, last_terminal_column)
        } else {
            0
        };
        self.terminal_views
            .get_mut(&view_id)
            .expect("active terminal view remains present")
            .selection = Some(selection);
    }

    fn reveal_terminal_position(&mut self, view_id: TerminalViewId, position: TerminalPosition) {
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let history_len = self
            .terminals
            .get(view.session_id)
            .map_or(0, |session| session.screen().scrollback().len());
        let visible_start = history_len.saturating_sub(view.scrollback);
        let page_rows = self.active_terminal_page_rows();
        let next = if position.row < visible_start {
            history_len.saturating_sub(position.row)
        } else if position.row >= visible_start.saturating_add(page_rows) {
            history_len.saturating_sub(position.row.saturating_sub(page_rows.saturating_sub(1)))
        } else {
            view.scrollback
        };
        self.set_active_terminal_scrollback(next);
    }

    fn yank_terminal_selection(&mut self) {
        let Some(view_id) = self.active_terminal_view_id() else {
            return;
        };
        let Some(view) = self.terminal_views.get(&view_id).copied() else {
            return;
        };
        let Some(selection) = view.selection else {
            return;
        };
        let Some(session) = self.terminals.get(view.session_id) else {
            return;
        };
        let bytes = terminal_selection_text(session.screen(), selection).into_bytes();
        self.register = Some(match selection.kind {
            SelectionKind::Character => Register::Character(bytes),
            SelectionKind::Line => Register::Line(bytes),
        });
        self.leave_terminal_visual();
        self.message.push_str(match selection.kind {
            SelectionKind::Character => "Terminal text yanked",
            SelectionKind::Line => "Terminal lines yanked",
        });
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
            Key::Char('v') => self.enter_visual(SelectionKind::Character),
            Key::Char('V') => self.enter_visual(SelectionKind::Line),
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
                self.enter_command_mode(None);
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

    fn handle_visual_key(&mut self, key: Key, kind: SelectionKind) -> Result<()> {
        if self.capture_count(&key) {
            return Ok(());
        }
        if self.pending.take() == Some(Pending::Go) && key == Key::Char('g') {
            self.count = None;
            self.editor.move_to_first_line();
            return Ok(());
        }

        let count = if key == Key::Char('g') {
            1
        } else {
            self.count.take().unwrap_or(1)
        };
        match key {
            Key::Escape | Key::Ctrl('c') => self.leave_visual(),
            Key::Char('v') if kind == SelectionKind::Character => self.leave_visual(),
            Key::Char('V') if kind == SelectionKind::Line => self.leave_visual(),
            Key::Char('v') => self.mode = Mode::Visual,
            Key::Char('V') => self.mode = Mode::VisualLine,
            Key::Char('h') | Key::ArrowLeft | Key::Modified(SpecialKey::ArrowLeft, _) => {
                for _ in 0..count {
                    if !self.editor.move_left() {
                        break;
                    }
                }
            }
            Key::Char('j') | Key::ArrowDown | Key::Modified(SpecialKey::ArrowDown, _) => {
                for _ in 0..count {
                    if !self.editor.move_down(false) {
                        break;
                    }
                }
            }
            Key::Char('k') | Key::ArrowUp | Key::Modified(SpecialKey::ArrowUp, _) => {
                for _ in 0..count {
                    if !self.editor.move_up(false) {
                        break;
                    }
                }
            }
            Key::Char('l') | Key::ArrowRight | Key::Modified(SpecialKey::ArrowRight, _) => {
                for _ in 0..count {
                    if !self.editor.move_right(false) {
                        break;
                    }
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
                    if !self.editor.move_word_forward() {
                        break;
                    }
                }
            }
            Key::Char('b') => {
                for _ in 0..count {
                    if !self.editor.move_word_backward() {
                        break;
                    }
                }
            }
            Key::Char('e') => {
                for _ in 0..count {
                    if !self.editor.move_word_end() {
                        break;
                    }
                }
            }
            Key::Char('g') => self.pending = Some(Pending::Go),
            Key::Char('G') => self.editor.move_to_last_line(),
            Key::Char('y') => self.yank_visual(kind),
            Key::Char('d') | Key::Char('x') | Key::Delete => self.delete_visual(kind),
            Key::Char('p') | Key::Char('P') => self.put_over_visual(kind)?,
            Key::Char(':') => self.enter_command_mode(Some(kind)),
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
                self.cancel_command_selection();
                self.mode = self.command_return_mode;
                self.command.clear();
            }
            Key::Backspace => {
                if self.command.pop().is_none() {
                    self.cancel_command_selection();
                    self.mode = self.command_return_mode;
                }
            }
            Key::Enter => {
                let command = std::mem::take(&mut self.command);
                self.mode = self.command_return_mode;
                self.execute_command(command.trim());
                self.editor.clear_selection();
                self.command_selection = None;
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
        let command = match parse_command(command) {
            Command::Substitute {
                range: SubstituteRange::CurrentLine,
                expression,
            } if self.command_selection.is_some() => Command::Substitute {
                range: SubstituteRange::SelectedLines,
                expression,
            },
            command => command,
        };
        let uses_selection = matches!(
            command,
            Command::Substitute {
                range: SubstituteRange::SelectedLines,
                ..
            }
        );
        if self.command_selection.is_some() && !uses_selection {
            self.editor.clear_selection();
        }
        match command {
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
            Command::Terminal(command) => self.open_terminal(command),
            Command::ListTerminals => self.list_terminal_sessions(),
            Command::AttachTerminal(Some(id)) => self.attach_terminal_id(id),
            Command::AttachTerminal(None) => self.message.push_str("Terminal session ID required"),
            Command::CloseTerminal {
                force,
                id: Some(id),
            } => {
                self.close_terminal_id(id, force);
            }
            Command::CloseTerminal { id: None, .. } => {
                self.message.push_str("Terminal session ID required");
            }
            Command::Substitute { range, expression } => self.execute_substitute(range, expression),
            Command::Empty => {}
            Command::Unknown(command) => self
                .message
                .push_str(&format!("Not an editor command: {command}")),
        }
    }

    fn quit_if_clean(&mut self) {
        match self.terminals.running_count() {
            Ok(count) if count > 0 => {
                self.message.push_str(&format!(
                    "{count} terminal session(s) still running (use :q! to terminate)"
                ));
                return;
            }
            Err(error) => {
                self.message
                    .push_str(&format!("Terminal status failed: {error:#}"));
                return;
            }
            Ok(_) => {}
        }
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

    fn open_terminal(&mut self, command: Option<&str>) {
        let size = match bed_terminal_session::PtySize::new(8, 80) {
            Ok(size) => size,
            Err(error) => {
                self.message
                    .push_str(&format!("Terminal size failed: {error:#}"));
                return;
            }
        };
        let session_id = match self
            .terminals
            .spawn_shell(command, size, TERMINAL_SCROLLBACK_ROWS)
        {
            Ok(id) => id,
            Err(error) => {
                self.message
                    .push_str(&format!("Terminal failed: {error:#}"));
                return;
            }
        };
        let view_id = TerminalViewId(self.next_terminal_view_id);
        self.next_terminal_view_id = self
            .next_terminal_view_id
            .checked_add(1)
            .expect("terminal view ID space exhausted");
        let cursor = self
            .terminals
            .get(session_id)
            .map_or(TerminalPosition::default(), |session| {
                terminal_live_position(session.screen())
            });
        self.terminal_views.insert(
            view_id,
            TerminalView {
                session_id,
                scrollback: 0,
                cursor,
                selection: None,
            },
        );

        self.open_terminal_view(view_id, true);
    }

    fn open_terminal_view(&mut self, view_id: TerminalViewId, input: bool) {
        debug_assert!(self.terminal_views.contains_key(&view_id));

        let source = self.active_window().view_id;
        let text_view = self
            .editor
            .duplicate_view(source)
            .expect("active window references a missing editor view");
        let buffer_id = self.editor.buffer_id();
        let window_id = self.allocate_window_id();
        let inserted = self
            .layout
            .split(self.active_window, window_id, SplitAxis::Rows);
        debug_assert!(inserted);
        let mut window = Window::new(buffer_id, text_view, Viewport::default());
        window.content = WindowContent::Terminal(view_id);
        self.windows.insert(window_id, window);
        self.activate_window(window_id);
        if input {
            self.mode = Mode::TerminalInput;
        }
    }

    fn terminal_session_id(&self, value: &str) -> Option<TerminalSessionId> {
        let number = value.parse::<u64>().ok()?;
        self.terminals.ids().find(|id| id.get() == number)
    }

    fn list_terminal_sessions(&mut self) {
        if let Err(error) = self.terminals.poll() {
            self.message
                .push_str(&format!("Terminal status failed: {error:#}"));
            return;
        }
        let ids: Vec<_> = self.terminals.ids().collect();
        if ids.is_empty() {
            self.message.push_str("No terminal sessions");
            return;
        }
        let active = self.active_terminal_session_id();
        for (index, id) in ids.into_iter().enumerate() {
            if index > 0 {
                self.message.push_str("  ");
            }
            let session = self
                .terminals
                .get(id)
                .expect("collected terminal session ID remains present");
            let marker = if active == Some(id) { '%' } else { ' ' };
            let state = session
                .status()
                .map_or_else(|| "running".to_owned(), |status| format!("exited {status}"));
            self.message.push_str(&format!(
                "{}:{marker} {} [{state}]",
                id.get(),
                session.command()
            ));
        }
    }

    fn attach_terminal_id(&mut self, value: &str) {
        let Some(session_id) = self.terminal_session_id(value) else {
            self.message
                .push_str(&format!("Terminal session {value} does not exist"));
            return;
        };
        let view_id = TerminalViewId(self.next_terminal_view_id);
        self.next_terminal_view_id = self
            .next_terminal_view_id
            .checked_add(1)
            .expect("terminal view ID space exhausted");
        let cursor = self
            .terminals
            .get(session_id)
            .map_or(TerminalPosition::default(), |session| {
                terminal_live_position(session.screen())
            });
        self.terminal_views.insert(
            view_id,
            TerminalView {
                session_id,
                scrollback: 0,
                cursor,
                selection: None,
            },
        );
        self.open_terminal_view(view_id, false);
    }

    fn close_terminal_id(&mut self, value: &str, force: bool) {
        let Some(session_id) = self.terminal_session_id(value) else {
            self.message
                .push_str(&format!("Terminal session {value} does not exist"));
            return;
        };
        let view_count = self
            .terminal_views
            .values()
            .filter(|view| view.session_id == session_id)
            .count();
        if view_count > 0 {
            self.message.push_str(&format!(
                "Terminal session {value} still has {view_count} view(s); close them first"
            ));
            return;
        }
        match self.terminals.close(session_id, force) {
            Ok(()) => self
                .message
                .push_str(&format!("Terminal session {value} closed")),
            Err(error) => self
                .message
                .push_str(&format!("Terminal close failed: {error:#}")),
        }
    }

    fn resize_terminal_view(&mut self, view_id: TerminalViewId, area: Rect) {
        let rows = area.rows.saturating_sub(1).max(1);
        let columns = area.columns.max(1);
        let (Ok(rows), Ok(columns)) = (u16::try_from(rows), u16::try_from(columns)) else {
            self.message.push_str("Terminal window is too large");
            return;
        };
        let Ok(size) = bed_terminal_session::PtySize::new(rows, columns) else {
            return;
        };
        let Some(session_id) = self
            .terminal_views
            .get(&view_id)
            .map(|view| view.session_id)
        else {
            return;
        };
        if let Some(session) = self.terminals.get_mut(session_id)
            && let Err(error) = session.resize(size)
        {
            self.message
                .push_str(&format!("Terminal resize failed: {error:#}"));
        }
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
        self.discard_window(window);
        self.set_mode_for_active_window();
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
                self.discard_window(window);
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
                self.set_mode_for_active_window();
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
                Key::Char('-') | Key::Char('+') | Key::Char('_') | Key::Ctrl('_') => {
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
            Key::Char('-') | Key::Ctrl('_') => {
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
        self.set_mode_for_active_window();
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
            let source_content = source.content;
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
            let content = match source_content {
                WindowContent::Text => WindowContent::Text,
                WindowContent::Terminal(source_view) => {
                    let source = *self
                        .terminal_views
                        .get(&source_view)
                        .expect("terminal window references a missing view");
                    let view_id = TerminalViewId(self.next_terminal_view_id);
                    self.next_terminal_view_id = self
                        .next_terminal_view_id
                        .checked_add(1)
                        .expect("terminal view ID space exhausted");
                    self.terminal_views.insert(view_id, source);
                    WindowContent::Terminal(view_id)
                }
            };
            let window_id = self.allocate_window_id();
            self.windows.insert(
                window_id,
                Window {
                    content,
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
        self.set_mode_for_active_window();
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
        self.set_mode_for_active_window();
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
                self.discard_window(window);
            }
        }
    }

    fn discard_window(&mut self, window: Window) {
        if let WindowContent::Terminal(view_id) = window.content {
            self.terminal_views.remove(&view_id);
        }
        for view_id in window.views.into_values() {
            self.editor.remove_view(view_id);
        }
    }

    fn set_mode_for_active_window(&mut self) {
        self.pending = None;
        self.terminal_prefix = false;
        self.mode = match self.active_window().content {
            WindowContent::Text => Mode::Normal,
            WindowContent::Terminal(_) => Mode::TerminalNormal,
        };
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

    fn enter_visual(&mut self, kind: SelectionKind) {
        self.editor.begin_selection();
        self.mode = match kind {
            SelectionKind::Character => Mode::Visual,
            SelectionKind::Line => Mode::VisualLine,
        };
    }

    fn leave_visual(&mut self) {
        self.editor.clear_selection();
        self.editor.normalize_normal_cursor();
        self.pending = None;
        self.count = None;
        self.mode = Mode::Normal;
    }

    fn yank_visual(&mut self, kind: SelectionKind) {
        let bytes = self.editor.selected_bytes(kind).unwrap_or_default();
        self.register = Some(match kind {
            SelectionKind::Character => Register::Character(bytes),
            SelectionKind::Line => Register::Line(bytes),
        });
        self.editor.finish_selection(kind);
        self.mode = Mode::Normal;
        self.message.push_str(match kind {
            SelectionKind::Character => "Text yanked",
            SelectionKind::Line => "Lines yanked",
        });
    }

    fn delete_visual(&mut self, kind: SelectionKind) {
        let bytes = self.editor.selected_bytes(kind).unwrap_or_default();
        let changes_document = self.editor.selection_range(kind).is_some_and(|range| {
            range.start < range.end || (kind == SelectionKind::Line && range.start > 0)
        });
        if changes_document {
            self.editor.checkpoint();
            self.editor.delete_selection(kind);
        } else {
            self.editor.finish_selection(kind);
        }
        self.register = Some(match kind {
            SelectionKind::Character => Register::Character(bytes),
            SelectionKind::Line => Register::Line(bytes),
        });
        self.mode = Mode::Normal;
    }

    fn put_over_visual(&mut self, kind: SelectionKind) -> Result<()> {
        let Some(replacement) = self.register.clone() else {
            self.message.push_str("Register is empty");
            return Ok(());
        };
        let selected = self.editor.selected_bytes(kind).unwrap_or_default();
        let selection = self.editor.selection_range(kind);
        let selected_to_end = selection
            .as_ref()
            .is_some_and(|range| range.end == self.editor.document().len());
        self.editor.checkpoint();
        self.editor.delete_selection(kind);

        match replacement {
            Register::Character(bytes) if selected_to_end && !self.editor.document().is_empty() => {
                self.editor.put_after(&bytes)?;
            }
            Register::Character(bytes) => {
                self.editor.put_before(&bytes)?;
            }
            Register::Line(bytes) if selected_to_end && !self.editor.document().is_empty() => {
                self.editor.put_line_below(&bytes)?;
            }
            Register::Line(bytes) => self.editor.put_line_above(&bytes)?,
        }
        self.editor.normalize_normal_cursor();
        self.register = Some(match kind {
            SelectionKind::Character => Register::Character(selected),
            SelectionKind::Line => Register::Line(selected),
        });
        self.mode = Mode::Normal;
        Ok(())
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

    fn enter_command_mode(&mut self, selection: Option<SelectionKind>) {
        self.command_return_mode = match self.mode {
            Mode::TerminalInput | Mode::TerminalNormal | Mode::TerminalVisual => self.mode,
            _ => Mode::Normal,
        };
        self.mode = Mode::Command;
        self.command.clear();
        self.command_selection = selection;
    }

    fn cancel_command_selection(&mut self) {
        if self.command_selection.take().is_some() {
            self.editor.clear_selection();
            self.editor.normalize_normal_cursor();
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
            (true, Mode::Visual) => "VISUAL",
            (true, Mode::VisualLine) => "VISUAL LINE",
            (true, Mode::Command) => "COMMAND",
            (true, Mode::Search) => "SEARCH",
            (true, Mode::Tree) => "TREE",
            (true, Mode::TerminalInput) => "TERMINAL INPUT",
            (true, Mode::TerminalNormal) => "TERMINAL NORMAL",
            (true, Mode::TerminalVisual) => "TERMINAL VISUAL",
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
        ("terminal" | "term", command) => Command::Terminal(command),
        ("terminals", None) => Command::ListTerminals,
        ("terminalattach", id) => Command::AttachTerminal(id),
        ("terminalclose", id) => Command::CloseTerminal { force: false, id },
        ("terminalclose!", id) => Command::CloseTerminal { force: true, id },
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
    render_text_with_selection(bytes, 0, None, column_offset, width)
}

fn render_text_with_selection(
    bytes: &[u8],
    document_offset: usize,
    selection: Option<(&std::ops::Range<usize>, SelectionKind)>,
    column_offset: usize,
    width: usize,
) -> String {
    let (text, source_boundaries) = lossy_text_with_source_boundaries(bytes);
    let mut output = String::new();
    let mut source_column = 0;
    let end = column_offset.saturating_add(width);
    let mut reverse = false;

    for (byte_offset, grapheme) in text.grapheme_indices(true) {
        let grapheme_width = grapheme_display_width(grapheme, source_column);
        let grapheme_end = source_column + grapheme_width;
        if grapheme_end > column_offset && source_column < end {
            let grapheme_range = document_offset + source_boundaries[byte_offset]
                ..document_offset + source_boundaries[byte_offset + grapheme.len()];
            let selected = selection.is_some_and(|(selection, _)| {
                grapheme_range.start < selection.end && grapheme_range.end > selection.start
            });
            if selected != reverse {
                output.push_str(if selected { "\x1b[7m" } else { "\x1b[m" });
                reverse = selected;
            }
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
    let line_end = document_offset + bytes.len();
    let selects_line_end = selection.is_some_and(|(selection, kind)| match kind {
        SelectionKind::Character => selection.start == line_end && selection.end == line_end,
        SelectionKind::Line => {
            (selection.start <= line_end && selection.end > document_offset)
                || (selection.is_empty() && selection.start == line_end)
        }
    });
    if selects_line_end && source_column >= column_offset && source_column < end {
        if !reverse {
            output.push_str("\x1b[7m");
            reverse = true;
        }
        output.push(' ');
    }
    if reverse {
        output.push_str("\x1b[m");
    }
    output
}

fn lossy_text_with_source_boundaries(bytes: &[u8]) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut source_boundaries = vec![0];
    let mut source_offset = 0;

    while source_offset < bytes.len() {
        let tail = &bytes[source_offset..];
        let (valid, invalid_length) = match std::str::from_utf8(tail) {
            Ok(valid) => (valid, None),
            Err(error) => {
                let valid_length = error.valid_up_to();
                let valid = std::str::from_utf8(&tail[..valid_length])
                    .expect("UTF-8 error reported a valid prefix");
                (
                    valid,
                    Some(error.error_len().unwrap_or(tail.len() - valid_length)),
                )
            }
        };

        for (relative_offset, character) in valid.char_indices() {
            let start = source_offset + relative_offset;
            push_mapped_character(
                &mut text,
                &mut source_boundaries,
                character,
                start,
                start + character.len_utf8(),
            );
        }
        source_offset += valid.len();

        let Some(invalid_length) = invalid_length else {
            break;
        };
        push_mapped_character(
            &mut text,
            &mut source_boundaries,
            '\u{fffd}',
            source_offset,
            source_offset + invalid_length,
        );
        source_offset += invalid_length;
    }

    (text, source_boundaries)
}

fn push_mapped_character(
    text: &mut String,
    source_boundaries: &mut Vec<usize>,
    character: char,
    source_start: usize,
    source_end: usize,
) {
    let decoded_start = text.len();
    text.push(character);
    source_boundaries[decoded_start] = source_start;
    source_boundaries.resize(text.len() + 1, source_start);
    source_boundaries[text.len()] = source_end;
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

fn render_terminal_row(
    output: &mut Vec<u8>,
    row: &Row,
    columns: usize,
    selection: Option<(usize, TerminalSelectionBounds)>,
) {
    let mut used = 0;
    let mut rendered = String::new();
    let mut attributes = None;
    for (column, cell) in row.cells().iter().enumerate() {
        if cell.is_continuation() || used >= columns {
            continue;
        }
        let width = UnicodeWidthStr::width(cell.contents()).max(1);
        if used + width > columns {
            break;
        }
        let selected = selection
            .is_some_and(|(row, selection)| terminal_cell_selected(row, column, width, selection));
        let mut next = cell.attributes();
        if selected {
            next.inverse = !next.inverse;
        }
        if attributes != Some(next) {
            output.extend_from_slice(RESET_STYLE);
            output.extend_from_slice(sgr_attributes(next).as_bytes());
            attributes = Some(next);
        }
        output.extend_from_slice(cell.contents().as_bytes());
        rendered.push_str(cell.contents());
        used += width;
    }
    output.extend_from_slice(RESET_STYLE);
    let display_used = UnicodeWidthStr::width(rendered.as_str()).min(columns);
    output.extend(std::iter::repeat_n(
        b' ',
        columns.saturating_sub(display_used),
    ));
}

fn terminal_display_column(row: &Row, column: usize) -> usize {
    let column = column.min(row.cells().len());
    if column < row.cells().len() && row.cells()[column].is_continuation() {
        let mut text = String::new();
        for cell in &row.cells()[..column.saturating_sub(1)] {
            if !cell.is_continuation() {
                text.push_str(cell.contents());
            }
        }
        return UnicodeWidthStr::width(text.as_str()).saturating_add(1);
    }

    let mut text = String::new();
    for cell in &row.cells()[..column] {
        if !cell.is_continuation() {
            text.push_str(cell.contents());
        }
    }
    UnicodeWidthStr::width(text.as_str())
}

#[derive(Clone, Copy)]
struct TerminalGraphemeSpan {
    start: usize,
    end: usize,
    blank: bool,
}

fn terminal_grapheme_spans(row: &Row) -> Vec<TerminalGraphemeSpan> {
    let mut text = String::new();
    let mut byte_columns = Vec::new();
    for (column, cell) in row.cells().iter().enumerate() {
        if cell.is_continuation() {
            continue;
        }
        text.push_str(cell.contents());
        byte_columns.extend(std::iter::repeat_n(column, cell.contents().len()));
    }

    text.grapheme_indices(true)
        .map(|(offset, grapheme)| {
            let start = byte_columns[offset];
            let last = byte_columns[offset + grapheme.len() - 1];
            let end = if last + 1 < row.cells().len() && row.cells()[last + 1].is_continuation() {
                last + 1
            } else {
                last
            };
            TerminalGraphemeSpan {
                start,
                end,
                blank: grapheme == " ",
            }
        })
        .collect()
}

fn terminal_grapheme_span(row: &Row, column: usize) -> Option<TerminalGraphemeSpan> {
    let spans = terminal_grapheme_spans(row);
    spans
        .iter()
        .copied()
        .find(|span| (span.start..=span.end).contains(&column))
        .or_else(|| spans.into_iter().rev().find(|span| span.start <= column))
}

fn terminal_row(screen: &Screen, row: usize) -> Option<&Row> {
    let history_len = screen.scrollback().len();
    if row < history_len {
        screen.scrollback().get(row)
    } else {
        screen.rows().get(row - history_len)
    }
}

fn last_terminal_column(row: &Row) -> usize {
    terminal_grapheme_spans(row)
        .into_iter()
        .rfind(|span| !span.blank)
        .map(|span| span.start)
        .unwrap_or(0)
}

fn normalize_terminal_position(
    screen: &Screen,
    mut position: TerminalPosition,
) -> TerminalPosition {
    let row_count = screen
        .scrollback()
        .len()
        .saturating_add(screen.rows().len());
    position.row = position.row.min(row_count.saturating_sub(1));
    let Some(row) = terminal_row(screen, position.row) else {
        return TerminalPosition::default();
    };
    position.column = position.column.min(row.cells().len().saturating_sub(1));
    if let Some(span) = terminal_grapheme_span(row, position.column) {
        position.column = span.start;
    }
    position
}

fn terminal_live_position(screen: &Screen) -> TerminalPosition {
    let cursor = screen.cursor();
    normalize_terminal_position(
        screen,
        TerminalPosition {
            row: screen.scrollback().len().saturating_add(cursor.row),
            column: cursor.column,
        },
    )
}

fn move_terminal_position(
    screen: &Screen,
    position: TerminalPosition,
    rows: isize,
    columns: isize,
) -> TerminalPosition {
    let row_count = screen
        .scrollback()
        .len()
        .saturating_add(screen.rows().len());
    let row = position
        .row
        .saturating_add_signed(rows)
        .min(row_count.saturating_sub(1));
    let Some(target) = terminal_row(screen, row) else {
        return position;
    };
    let spans = terminal_grapheme_spans(target);
    let current = position.column.min(target.cells().len().saturating_sub(1));
    let current_index = spans
        .iter()
        .position(|span| (span.start..=span.end).contains(&current))
        .or_else(|| spans.iter().rposition(|span| span.start <= current))
        .unwrap_or(0);
    let column = if columns == 0 {
        spans.get(current_index).map_or(0, |span| span.start)
    } else {
        let index = current_index
            .saturating_add_signed(columns)
            .min(spans.len().saturating_sub(1));
        spans.get(index).map_or(0, |span| span.start)
    };
    TerminalPosition { row, column }
}

fn terminal_selection_bounds(
    screen: &Screen,
    selection: TerminalSelection,
) -> TerminalSelectionBounds {
    let (mut start, mut end) = if selection.anchor <= selection.cursor {
        (selection.anchor, selection.cursor)
    } else {
        (selection.cursor, selection.anchor)
    };
    if selection.kind == SelectionKind::Line {
        while start.row > 0 && terminal_row(screen, start.row - 1).is_some_and(Row::wrapped) {
            start.row -= 1;
        }
        let row_count = screen
            .scrollback()
            .len()
            .saturating_add(screen.rows().len());
        while end.row.saturating_add(1) < row_count
            && terminal_row(screen, end.row).is_some_and(Row::wrapped)
        {
            end.row += 1;
        }
        start.column = 0;
        end.column = usize::MAX;
    } else {
        if let Some(row) = terminal_row(screen, start.row)
            && let Some(span) = terminal_grapheme_span(row, start.column)
        {
            start.column = span.start;
        }
        if let Some(row) = terminal_row(screen, end.row)
            && let Some(span) = terminal_grapheme_span(row, end.column)
        {
            end.column = span.end;
        }
    }
    TerminalSelectionBounds { start, end }
}

fn shift_terminal_selection(
    mut selection: TerminalSelection,
    discarded_rows: usize,
) -> Option<TerminalSelection> {
    if selection.anchor.row < discarded_rows && selection.cursor.row < discarded_rows {
        return None;
    }
    selection.anchor.row = selection.anchor.row.saturating_sub(discarded_rows);
    selection.cursor.row = selection.cursor.row.saturating_sub(discarded_rows);
    Some(selection)
}

fn terminal_cell_selected(
    row: usize,
    column: usize,
    width: usize,
    selection: TerminalSelectionBounds,
) -> bool {
    let cell_end = column.saturating_add(width.saturating_sub(1));
    (row > selection.start.row || cell_end >= selection.start.column)
        && (row < selection.end.row || column <= selection.end.column)
        && (selection.start.row..=selection.end.row).contains(&row)
}

fn terminal_selection_text(screen: &Screen, selection: TerminalSelection) -> String {
    let TerminalSelectionBounds { start, end } = terminal_selection_bounds(screen, selection);
    let mut output = String::new();
    for row_index in start.row..=end.row {
        let Some(row) = terminal_row(screen, row_index) else {
            break;
        };
        let start_column = if row_index == start.row {
            start.column
        } else {
            0
        };
        let end_column = if row_index == end.row {
            end.column
        } else {
            row.cells().len().saturating_sub(1)
        };
        let mut line = String::new();
        for (column, cell) in row.cells().iter().enumerate() {
            if cell.is_continuation() {
                continue;
            }
            let width = UnicodeWidthStr::width(cell.contents()).max(1);
            let cell_end = column.saturating_add(width.saturating_sub(1));
            if cell_end >= start_column && column <= end_column {
                line.push_str(cell.contents());
            }
        }
        if row_index < end.row && row.wrapped() {
            output.push_str(&line);
        } else {
            output.push_str(line.trim_end_matches(' '));
            if row_index < end.row {
                output.push('\n');
            }
        }
    }
    output
}

fn anchored_terminal_scrollback(
    current: usize,
    history_before: u64,
    history_after: u64,
    maximum: usize,
) -> usize {
    if current == 0 {
        return 0;
    }
    let advanced =
        usize::try_from(history_after.saturating_sub(history_before)).unwrap_or(usize::MAX);
    current.saturating_add(advanced).min(maximum)
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
            parameters.push(format!("{};5;{index}", if foreground { 38 } else { 48 }))
        }
        Color::Rgb(red, green, blue) => parameters.push(format!(
            "{};2;{red};{green};{blue}",
            if foreground { 38 } else { 48 }
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, Command, DEFAULT_FILE_TREE_WIDTH, Mode, ParsedSubstitute, SplitAxis, TerminalPosition,
        TerminalSelection, anchored_terminal_scrollback, display_width, move_terminal_position,
        parse_command, parse_substitute_expression, render_terminal_row, render_text,
        render_text_with_selection, sgr_attributes, shift_terminal_selection,
        terminal_display_column, terminal_live_position, terminal_selection_bounds,
        terminal_selection_text,
    };
    use bed_core::{Document, Editor, SelectionKind, SubstituteOptions, SubstituteRange};
    use bed_terminal::{Key, TerminalSize};
    use bed_vt100::{Attributes, Color, TerminalEmulator};
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

    #[test]
    fn maps_terminal_cell_attributes_to_outer_sgr() {
        assert_eq!(
            sgr_attributes(Attributes {
                foreground: Color::Indexed(9),
                background: Color::Rgb(1, 2, 3),
                bold: true,
                underline: true,
                ..Attributes::default()
            }),
            "\x1b[1;4;91;48;2;1;2;3m"
        );
        assert_eq!(
            sgr_attributes(Attributes {
                foreground: Color::Indexed(200),
                ..Attributes::default()
            }),
            "\x1b[38;5;200m"
        );
    }

    #[test]
    fn terminal_selection_handles_wide_cells_reverse_ranges_and_soft_wraps() {
        let mut terminal = TerminalEmulator::new(3, 5, 10);
        terminal.process("a好b\r\nabcdeX".as_bytes());
        let history = terminal.screen().scrollback().len();

        assert_eq!(
            terminal_selection_text(
                terminal.screen(),
                TerminalSelection {
                    anchor: TerminalPosition {
                        row: history + 1,
                        column: 4,
                    },
                    cursor: TerminalPosition {
                        row: history,
                        column: 2,
                    },
                    kind: SelectionKind::Character,
                },
            ),
            "好b\nabcde"
        );
        assert!(terminal.screen().rows()[1].wrapped());
        assert_eq!(
            terminal_selection_text(
                terminal.screen(),
                TerminalSelection {
                    anchor: TerminalPosition {
                        row: history + 1,
                        column: 3,
                    },
                    cursor: TerminalPosition {
                        row: history + 2,
                        column: 0,
                    },
                    kind: SelectionKind::Character,
                },
            ),
            "deX"
        );

        let wide = TerminalPosition {
            row: history,
            column: 1,
        };
        assert_eq!(
            move_terminal_position(terminal.screen(), wide, 0, 1).column,
            3
        );
        assert_eq!(
            move_terminal_position(
                terminal.screen(),
                TerminalPosition {
                    row: history,
                    column: 3,
                },
                0,
                -1,
            )
            .column,
            1
        );

        let mut spaces = TerminalEmulator::new(2, 5, 0);
        spaces.process(b"abc  X");
        assert_eq!(
            terminal_selection_text(
                spaces.screen(),
                TerminalSelection {
                    anchor: TerminalPosition { row: 0, column: 0 },
                    cursor: TerminalPosition { row: 1, column: 0 },
                    kind: SelectionKind::Character,
                },
            ),
            "abc  X"
        );
    }

    #[test]
    fn terminal_line_selection_expands_across_soft_wrapped_logical_lines() {
        let mut terminal = TerminalEmulator::new(4, 5, 0);
        terminal.process(b"abcdeX\r\nlast");
        assert!(terminal.screen().rows()[0].wrapped());

        let wrapped_line = TerminalSelection {
            anchor: TerminalPosition { row: 1, column: 0 },
            cursor: TerminalPosition { row: 1, column: 0 },
            kind: SelectionKind::Line,
        };
        assert_eq!(
            terminal_selection_bounds(terminal.screen(), wrapped_line),
            super::TerminalSelectionBounds {
                start: TerminalPosition { row: 0, column: 0 },
                end: TerminalPosition {
                    row: 1,
                    column: usize::MAX,
                },
            }
        );
        assert_eq!(
            terminal_selection_text(terminal.screen(), wrapped_line),
            "abcdeX"
        );

        let reverse_lines = TerminalSelection {
            anchor: TerminalPosition { row: 2, column: 3 },
            cursor: TerminalPosition { row: 1, column: 0 },
            kind: SelectionKind::Line,
        };
        assert_eq!(
            terminal_selection_text(terminal.screen(), reverse_lines),
            "abcdeX\nlast"
        );
    }

    #[test]
    fn terminal_selection_rendering_toggles_existing_inverse_cells() {
        let mut terminal = TerminalEmulator::new(1, 4, 0);
        terminal.process(b"a\x1b[7mb\x1b[0mc");
        let mut output = Vec::new();
        render_terminal_row(
            &mut output,
            terminal.screen().row(0).unwrap(),
            4,
            Some((
                0,
                terminal_selection_bounds(
                    terminal.screen(),
                    TerminalSelection {
                        anchor: TerminalPosition { row: 0, column: 0 },
                        cursor: TerminalPosition { row: 0, column: 2 },
                        kind: SelectionKind::Character,
                    },
                ),
            )),
        );
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("\x1b[7ma"));
        assert!(rendered.contains("\x1b[mb"));
        assert!(rendered.contains("\x1b[7mc"));
    }

    #[test]
    fn terminal_selection_is_clamped_or_cleared_when_history_is_discarded() {
        let selection = TerminalSelection {
            anchor: TerminalPosition { row: 2, column: 1 },
            cursor: TerminalPosition { row: 5, column: 3 },
            kind: SelectionKind::Line,
        };
        assert_eq!(
            shift_terminal_selection(selection, 3),
            Some(TerminalSelection {
                anchor: TerminalPosition { row: 0, column: 1 },
                cursor: TerminalPosition { row: 2, column: 3 },
                kind: SelectionKind::Line,
            })
        );
        assert_eq!(shift_terminal_selection(selection, 6), None);
    }

    #[test]
    fn parses_terminal_commands_without_vim_aliases() {
        assert_eq!(parse_command("terminal"), Command::Terminal(None));
        assert_eq!(
            parse_command("term printf ready"),
            Command::Terminal(Some("printf ready"))
        );
        assert_eq!(parse_command("te"), Command::Unknown("te"));
        assert_eq!(parse_command("terminals"), Command::ListTerminals);
        assert_eq!(
            parse_command("terminalattach 12"),
            Command::AttachTerminal(Some("12"))
        );
        assert_eq!(
            parse_command("terminalclose 12"),
            Command::CloseTerminal {
                force: false,
                id: Some("12")
            }
        );
        assert_eq!(
            parse_command("terminalclose! 12"),
            Command::CloseTerminal {
                force: true,
                id: Some("12")
            }
        );
    }

    fn execute(app: &mut App, command: &str) {
        app.handle_key(Key::Char(':')).unwrap();
        for character in command.chars() {
            app.handle_key(Key::Char(character)).unwrap();
        }
        app.handle_key(Key::Enter).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn opens_renders_and_detaches_a_terminal_session() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"text");
        app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });

        execute(&mut app, "terminal printf '\\033[31mREADY\\033[0m'");

        assert_eq!(app.mode(), Mode::TerminalInput);
        assert_eq!(app.layout.windows().len(), 2);
        let view_id = match app.active_window().content {
            super::WindowContent::Terminal(view_id) => view_id,
            super::WindowContent::Text => panic!("terminal command focused a text window"),
        };
        let session_id = app.terminal_views[&view_id].session_id;
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            session.status().is_none()
                || !session.reached_eof()
                || !session.screen().contents().contains("READY")
        }) {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "terminal command output did not finish"
            );
            thread::sleep(Duration::from_millis(2));
        }

        let frame = app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        assert!(frame.windows(b"READY".len()).any(|bytes| bytes == b"READY"));
        assert!(frame.windows(5).any(|bytes| bytes == b"\x1b[31m"));
        assert!(
            frame
                .windows(b"TERMINAL NORMAL printf".len())
                .any(|bytes| bytes == b"TERMINAL NORMAL printf")
        );
        assert!(frame.windows(7).any(|bytes| bytes == b"[exited"));

        app.close_active_window();
        assert_eq!(app.mode(), Mode::Normal);
        assert!(!app.terminal_views.contains_key(&view_id));
        assert!(app.terminals.get(session_id).is_some());

        execute(&mut app, "terminals");
        assert!(app.message.contains(&format!("{}: ", session_id.get())));
        assert!(app.message.contains("[exited"));
        execute(&mut app, &format!("terminalattach {}", session_id.get()));
        assert_eq!(app.mode(), Mode::TerminalNormal);
        assert_eq!(app.active_terminal_session_id(), Some(session_id));
        execute(&mut app, &format!("terminalclose {}", session_id.get()));
        assert!(app.message.contains("still has 1 view(s)"));
        app.close_active_window();
        execute(&mut app, &format!("terminalclose {}", session_id.get()));
        assert!(app.terminals.get(session_id).is_none());
        assert!(app.message.contains("closed"));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_title_and_bells_surface_child_feedback() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        execute(
            &mut app,
            "terminal printf '\\033]2;child title\\033\\\\\\007\\007\\033gREADY'",
        );
        let session_id = app.active_terminal_session_id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            session.status().is_none()
                || !session.reached_eof()
                || !session.screen().contents().contains("READY")
        }) {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "terminal feedback did not finish"
            );
            thread::sleep(Duration::from_millis(2));
        }

        let frame = app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        assert!(
            frame
                .windows(b"TERMINAL NORMAL child title".len())
                .any(|bytes| bytes == b"TERMINAL NORMAL child title")
        );
        assert!(
            app.message
                .contains(&format!("Terminal {} bell", session_id.get()))
        );
        assert!(
            app.message
                .contains(&format!("Terminal {} visual bell", session_id.get()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_input_respects_child_cursor_visibility() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        execute(&mut app, "terminal printf '\\033[?25lREADY'; sleep 30");
        let session_id = app.active_terminal_session_id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            session.screen().cursor().visible || !session.screen().contents().contains("READY")
        }) {
            app.poll_terminals().unwrap();
            assert!(Instant::now() < deadline, "child did not hide its cursor");
            thread::sleep(Duration::from_millis(2));
        }

        let frame = app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        assert!(frame.ends_with(b"\x1b[6 q\x1b[?25l"));

        app.handle_key(Key::Ctrl('\\')).unwrap();
        app.handle_key(Key::Ctrl('n')).unwrap();
        let frame = app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        assert!(frame.ends_with(b"\x1b[2 q\x1b[?25h"));
        app.terminals.close(session_id, true).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn alternate_screen_transition_clears_view_local_history_state() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        execute(
            &mut app,
            concat!(
                "terminal stty raw -echo; printf '",
                "01\\r\\n02\\r\\n03\\r\\n04\\r\\n05\\r\\n06\\r\\n07\\r\\n08\\r\\n",
                "09\\r\\n10\\r\\n11\\r\\n12\\r\\n13\\r\\n14\\r\\n15\\r\\n16\\r\\n",
                "17\\r\\n18\\r\\n19\\r\\n20\\r\\nREADY'; ",
                "dd bs=1 count=1 2>/dev/null; printf '\\033[?1049hALT'; sleep 30"
            ),
        );
        let view_id = app.active_terminal_view_id().unwrap();
        let session_id = app.active_terminal_session_id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            !session.screen().contents().contains("READY") || session.scrollback_len() == 0
        }) {
            app.poll_terminals().unwrap();
            assert!(Instant::now() < deadline, "child did not fill history");
            thread::sleep(Duration::from_millis(2));
        }

        app.handle_key(Key::Ctrl('\\')).unwrap();
        app.handle_key(Key::Ctrl('n')).unwrap();
        let live_cursor = app.terminal_views[&view_id].cursor;
        app.handle_key(Key::Char('k')).unwrap();
        assert!(app.terminal_views[&view_id].cursor.row < live_cursor.row);
        app.handle_key(Key::PageUp).unwrap();
        app.handle_key(Key::Char('v')).unwrap();
        assert!(app.terminal_views[&view_id].scrollback > 0);
        let selection = app.terminal_views[&view_id].selection.unwrap();
        assert_eq!(selection.anchor, app.terminal_views[&view_id].cursor);
        assert_eq!(selection.cursor, app.terminal_views[&view_id].cursor);
        app.terminals
            .get(session_id)
            .unwrap()
            .send_bytes(vec![b'x'])
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .terminals
            .get(session_id)
            .is_some_and(|session| !session.screen().contents().contains("ALT"))
        {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "child did not enter alternate screen"
            );
            thread::sleep(Duration::from_millis(2));
        }
        app.poll_terminals().unwrap();

        assert_eq!(app.mode(), Mode::TerminalNormal);
        assert_eq!(app.terminal_views[&view_id].scrollback, 0);
        assert!(app.terminal_views[&view_id].selection.is_none());
        app.terminals.close(session_id, true).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn terminal_mouse_uses_child_coordinates_and_ignores_status_rows() {
        use bed_terminal::{Modifiers, MouseAction, MouseButton, MouseEvent};
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        execute(
            &mut app,
            concat!(
                "terminal stty raw -echo; ",
                "printf '\\033[?1000h\\033[?1006hMOUSE_READY'; ",
                "bytes=$(dd bs=1 count=9 2>/dev/null | od -An -tx1 | tr -d ' \\n'); ",
                "[ \"$bytes\" = 1b5b3c303b333b324d ] && printf '\\r\\nMOUSE_OK\\r\\n'"
            ),
        );
        app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        let session_id = app.active_terminal_session_id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .terminals
            .get(session_id)
            .is_some_and(|session| session.modes().mouse_tracking != Some(1000))
        {
            app.poll_terminals().unwrap();
            assert!(Instant::now() < deadline, "mouse mode was not enabled");
            thread::sleep(Duration::from_millis(2));
        }

        let area = app
            .layout
            .rectangles(app.editor_area())
            .into_iter()
            .find(|(id, _)| *id == app.active_window)
            .unwrap()
            .1;
        app.handle_mouse(MouseEvent {
            row: area.row + area.rows - 1,
            column: area.column + 2,
            action: MouseAction::Press(MouseButton::Left),
            modifiers: Modifiers::default(),
        });
        app.handle_key(Key::Ctrl('\\')).unwrap();
        app.handle_key(Key::Ctrl('n')).unwrap();
        assert!(!app.handle_mouse(MouseEvent {
            row: area.row + 1,
            column: area.column + 2,
            action: MouseAction::Press(MouseButton::Left),
            modifiers: Modifiers::default(),
        }));
        app.handle_key(Key::Char('i')).unwrap();
        assert!(!app.handle_mouse(MouseEvent {
            row: area.row + 1,
            column: area.column + 2,
            action: MouseAction::Press(MouseButton::Left),
            modifiers: Modifiers::default(),
        }));

        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            session.status().is_none() || !session.screen().contents().contains("MOUSE_OK")
        }) {
            app.poll_terminals().unwrap();
            assert!(Instant::now() < deadline, "mouse input did not reach child");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            app.terminals
                .get(session_id)
                .unwrap()
                .screen()
                .contents()
                .contains("MOUSE_OK")
        );
    }

    #[cfg(unix)]
    #[test]
    fn running_terminal_sessions_require_forced_cleanup_after_detach() {
        let mut app = app_with(b"");
        execute(&mut app, "terminal sleep 30");
        let session_id = app.active_terminal_session_id().unwrap();
        app.close_active_window();

        execute(&mut app, &format!("terminalclose {}", session_id.get()));
        assert!(app.terminals.get(session_id).is_some());
        assert!(app.message.contains("still running"));

        execute(&mut app, &format!("terminalclose! {}", session_id.get()));
        assert!(app.terminals.get(session_id).is_none());
        assert!(app.message.contains("closed"));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_visual_yanks_into_the_shared_bed_register() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        execute(&mut app, "terminal printf COPY");
        let session_id = app.active_terminal_session_id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            session.status().is_none()
                || !session.reached_eof()
                || !session.screen().contents().contains("COPY")
        }) {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "terminal copy output did not finish"
            );
            thread::sleep(Duration::from_millis(2));
        }

        app.handle_key(Key::Char('v')).unwrap();
        assert_eq!(app.mode(), Mode::TerminalVisual);
        app.handle_key(Key::Char('0')).unwrap();
        app.handle_key(Key::Char('y')).unwrap();
        assert_eq!(app.mode(), Mode::TerminalNormal);
        assert_eq!(
            app.register,
            Some(super::Register::Character(b"COPY".to_vec()))
        );

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('w')).unwrap();
        app.handle_key(Key::Char('p')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"COPY");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_visual_line_switches_kinds_and_yanks_a_line_register() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        execute(&mut app, "terminal printf 'one\\r\\ntwo'");
        let session_id = app.active_terminal_session_id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            session.status().is_none()
                || !session.reached_eof()
                || !session.screen().contents().contains("two")
        }) {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "terminal line selection output did not finish"
            );
            thread::sleep(Duration::from_millis(2));
        }

        app.handle_key(Key::Char('V')).unwrap();
        assert_eq!(app.mode(), Mode::TerminalVisual);
        let view_id = app.active_terminal_view_id().unwrap();
        assert_eq!(
            app.terminal_views[&view_id].selection.unwrap().kind,
            SelectionKind::Line
        );
        let frame = String::from_utf8(app.render(TerminalSize {
            rows: 20,
            columns: 80,
        }))
        .unwrap();
        assert!(frame.contains("TERMINAL VISUAL LINE"));

        app.handle_key(Key::Char('V')).unwrap();
        assert_eq!(app.mode(), Mode::TerminalNormal);
        app.handle_key(Key::Char('V')).unwrap();
        app.handle_key(Key::Char('v')).unwrap();
        assert_eq!(
            app.terminal_views[&view_id].selection.unwrap().kind,
            SelectionKind::Character
        );
        app.handle_key(Key::Char('V')).unwrap();
        app.handle_key(Key::Char('y')).unwrap();

        assert_eq!(app.mode(), Mode::TerminalNormal);
        assert_eq!(app.register, Some(super::Register::Line(b"two".to_vec())));
        assert!(app.message.contains("Terminal lines yanked"));
    }

    #[cfg(unix)]
    #[test]
    fn normal_quit_protects_a_running_terminal_session() {
        let mut app = app_with(b"");
        execute(&mut app, "terminal sleep 30");
        app.handle_key(Key::Ctrl('\\')).unwrap();
        app.handle_key(Key::Ctrl('n')).unwrap();

        execute(&mut app, "q");

        assert!(!app.should_quit());
        assert!(app.message.contains("terminal session(s) still running"));
        execute(&mut app, "q!");
        assert!(app.should_quit());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_input_and_normal_modes_route_keys_deliberately() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        execute(
            &mut app,
            "terminal stty raw -echo; printf 'INPUT_READY\\r\\n'; bytes=$(dd bs=1 count=7 2>/dev/null | od -An -tx1 | tr -d ' \\n'); [ \"$bytes\" = 68656c6c6f0d1c ] && printf '\\r\\nINPUT_OK\\r\\n'",
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .active_terminal_session_id()
            .and_then(|id| app.terminals.get(id))
            .is_some_and(|session| !session.screen().contents().contains("INPUT_READY"))
        {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "terminal child did not become ready"
            );
            thread::sleep(Duration::from_millis(2));
        }
        for (index, key) in [
            Key::Char('h'),
            Key::Char('e'),
            Key::Char('l'),
            Key::Char('l'),
            Key::Char('o'),
            Key::Enter,
            Key::Ctrl('\\'),
            Key::Ctrl('\\'),
            Key::Ctrl('\\'),
            Key::Ctrl('n'),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(app.handle_key(key).unwrap(), index == 9);
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .active_terminal_session_id()
            .and_then(|id| app.terminals.get(id))
            .is_some_and(|session| {
                session.status().is_none()
                    || !session.reached_eof()
                    || !session.screen().contents().contains("INPUT_OK")
            })
        {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "terminal input child did not exit"
            );
            thread::sleep(Duration::from_millis(2));
        }
        let session_id = app.active_terminal_session_id().unwrap();
        assert!(
            app.terminals
                .get(session_id)
                .unwrap()
                .screen()
                .contents()
                .contains("INPUT_OK")
        );
        assert_eq!(app.mode(), Mode::TerminalNormal);

        app.handle_key(Key::Char('k')).unwrap();
        app.handle_key(Key::Ctrl('j')).unwrap();
        assert_eq!(app.mode(), Mode::TerminalNormal);
        assert!(app.message.contains("Invalid Terminal Normal command"));
        app.message.clear();
        app.handle_key(Key::Char('i')).unwrap();
        assert_eq!(app.mode(), Mode::TerminalInput);
        assert_eq!(
            app.terminal_views[&app.active_terminal_view_id().unwrap()].scrollback,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_terminal_view_stays_at_the_bottom_during_output_floods() {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = app_with(b"");
        execute(&mut app, "terminal seq 1 12000");
        let view_id = app.active_terminal_view_id().unwrap();
        let session_id = app.active_terminal_session_id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.terminals.get(session_id).is_some_and(|session| {
            session.status().is_none()
                || !session.reached_eof()
                || !session.screen().contents().contains("12000")
        }) {
            app.poll_terminals().unwrap();
            assert!(
                Instant::now() < deadline,
                "terminal output flood did not finish"
            );
            thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(app.terminal_views[&view_id].scrollback, 0);
        assert_eq!(app.mode(), Mode::TerminalNormal);
        let session = app.terminals.get(session_id).unwrap();
        assert_eq!(
            app.terminal_views[&view_id].cursor,
            terminal_live_position(session.screen())
        );
        assert_eq!(app.terminal_views[&view_id].scrollback, 0);
    }

    #[test]
    fn anchors_bounded_terminal_scrollback_as_history_advances() {
        assert_eq!(anchored_terminal_scrollback(0, 10, 12, 10), 0);
        assert_eq!(anchored_terminal_scrollback(3, 10, 12, 10), 5);
        assert_eq!(anchored_terminal_scrollback(9, 10, 12, 10), 10);
        assert_eq!(anchored_terminal_scrollback(9, 12, 3, 4), 4);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_window_prefix_restores_modes_and_focused_resize_ownership() {
        let mut app = app_with(b"");
        app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        execute(&mut app, "terminal sleep 30");
        app.render(TerminalSize {
            rows: 20,
            columns: 80,
        });
        let session_id = app.active_terminal_session_id().unwrap();
        let terminal_size = app.terminals.get(session_id).unwrap().size();

        app.handle_key(Key::Ctrl('\\')).unwrap();
        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('w')).unwrap();
        assert_eq!(app.mode(), Mode::Normal);
        assert!(!app.terminal_prefix);
        app.render(TerminalSize {
            rows: 30,
            columns: 100,
        });
        assert_eq!(app.terminals.get(session_id).unwrap().size(), terminal_size);

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('w')).unwrap();
        assert_eq!(app.mode(), Mode::TerminalNormal);
        app.render(TerminalSize {
            rows: 30,
            columns: 100,
        });
        assert_ne!(app.terminals.get(session_id).unwrap().size(), terminal_size);
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
    fn visual_character_mode_selects_graphemes_and_yanks_them() {
        let mut app = app_with("a👩🏽‍💻b".as_bytes());
        app.handle_key(Key::Char('v')).unwrap();
        app.handle_key(Key::Char('l')).unwrap();

        let frame = String::from_utf8(app.render(TerminalSize {
            rows: 6,
            columns: 30,
        }))
        .unwrap();
        assert_eq!(app.mode(), Mode::Visual);
        assert!(frame.contains("VISUAL"));
        assert!(frame.contains("\x1b[7ma👩🏽‍💻\x1b[m"));

        app.handle_key(Key::Char('y')).unwrap();
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.editor().cursor().offset(), 0);
        app.handle_key(Key::Char('$')).unwrap();
        app.handle_key(Key::Char('P')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), "a👩🏽‍💻a👩🏽‍💻b".as_bytes());
    }

    #[test]
    fn visual_line_mode_deletes_crlf_lines_as_one_change() {
        let mut app = app_with(b"one\r\ntwo\r\nthree");
        app.handle_key(Key::Char('V')).unwrap();
        app.handle_key(Key::Char('j')).unwrap();
        assert_eq!(app.mode(), Mode::VisualLine);

        app.handle_key(Key::Char('d')).unwrap();

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.editor().document().as_bytes(), b"three");
        app.handle_key(Key::Char('u')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one\r\ntwo\r\nthree");
    }

    #[test]
    fn visual_put_replaces_selection_and_keeps_deleted_text_in_the_register() {
        let mut app = app_with(b"one two");
        for key in [Key::Char('v'), Key::Char('e'), Key::Char('y')] {
            app.handle_key(key).unwrap();
        }
        app.handle_key(Key::Char('w')).unwrap();
        for key in [Key::Char('v'), Key::Char('e'), Key::Char('p')] {
            app.handle_key(key).unwrap();
        }

        assert_eq!(app.editor().document().as_bytes(), b"one one");
        app.handle_key(Key::Char('$')).unwrap();
        app.handle_key(Key::Char('p')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one onetwo");
        app.handle_key(Key::Char('u')).unwrap();
        assert_eq!(app.editor().document().as_bytes(), b"one one");
    }

    #[test]
    fn visual_put_with_an_empty_register_keeps_the_selection() {
        let mut app = app_with(b"one two");
        app.handle_key(Key::Char('v')).unwrap();
        app.handle_key(Key::Char('e')).unwrap();
        let selection = app
            .editor()
            .selection_range(SelectionKind::Character)
            .unwrap();

        app.handle_key(Key::Char('p')).unwrap();

        assert_eq!(app.mode(), Mode::Visual);
        assert_eq!(
            app.editor().selection_range(SelectionKind::Character),
            Some(selection)
        );
        assert_eq!(app.editor().document().as_bytes(), b"one two");
    }

    #[test]
    fn visual_substitute_targets_selected_lines_without_vim_markers() {
        let mut app = app_with(b"x=1\nx=2\nx=3");
        app.handle_key(Key::Char('V')).unwrap();
        app.handle_key(Key::Char('j')).unwrap();
        app.handle_key(Key::Char(':')).unwrap();
        assert_eq!(app.mode(), Mode::Command);
        assert!(app.command.is_empty());
        for character in "s/(x)/$1$1/".chars() {
            app.handle_key(Key::Char(character)).unwrap();
        }
        app.handle_key(Key::Enter).unwrap();

        assert_eq!(app.editor().document().as_bytes(), b"xx=1\nxx=2\nx=3");
        assert_eq!(app.editor().view().selection_anchor(), None);
    }

    #[test]
    fn explicit_whole_buffer_substitute_ignores_the_visual_range() {
        let mut app = app_with(b"x=1\nx=2\nx=3");
        app.handle_key(Key::Char('V')).unwrap();
        app.handle_key(Key::Char('j')).unwrap();
        app.handle_key(Key::Char(':')).unwrap();
        for character in "%s/x/y/".chars() {
            app.handle_key(Key::Char(character)).unwrap();
        }
        app.handle_key(Key::Enter).unwrap();

        assert_eq!(app.editor().document().as_bytes(), b"y=1\ny=2\ny=3");
        assert_eq!(app.editor().view().selection_anchor(), None);
    }

    #[test]
    fn cancelling_a_visual_command_clears_the_selection() {
        let mut app = app_with(b"one two");
        app.handle_key(Key::Char('v')).unwrap();
        app.handle_key(Key::Char('e')).unwrap();
        app.handle_key(Key::Char(':')).unwrap();
        app.handle_key(Key::Escape).unwrap();

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.editor().view().selection_anchor(), None);
    }

    #[test]
    fn non_selection_commands_leave_visual_without_stale_state() {
        let mut app = app_with(b"one two");
        app.handle_key(Key::Char('v')).unwrap();
        app.handle_key(Key::Char('e')).unwrap();
        app.handle_key(Key::Char(':')).unwrap();
        for character in "buffers".chars() {
            app.handle_key(Key::Char(character)).unwrap();
        }
        app.handle_key(Key::Enter).unwrap();

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.editor().view().selection_anchor(), None);
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

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Char('_')).unwrap();
        let maximized_rows = app.layout.rectangles(app.editor_area())[2].1.rows;
        assert!(maximized_rows > 5);

        app.handle_key(Key::Ctrl('w')).unwrap();
        app.handle_key(Key::Ctrl('_')).unwrap();
        assert_eq!(
            app.layout.rectangles(app.editor_area())[2].1.rows,
            maximized_rows - 1
        );
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
        assert!(frame.ends_with("\x1b[2;3H\x1b[2 q\x1b[?25h"));
    }

    #[test]
    fn invalid_utf8_selection_uses_original_document_offsets() {
        let bytes = [0xff, b'a'];

        assert_eq!(
            render_text_with_selection(&bytes, 0, Some((&(1..2), SelectionKind::Character)), 0, 10,),
            "�\x1b[7ma\x1b[m"
        );
        assert_eq!(
            render_text_with_selection(&bytes, 0, Some((&(0..1), SelectionKind::Character)), 0, 10,),
            "\x1b[7m�\x1b[ma"
        );
    }

    #[test]
    fn selection_rendering_respects_horizontal_clipping() {
        assert_eq!(
            render_text_with_selection(
                "a好b".as_bytes(),
                0,
                Some((&(1..4), SelectionKind::Character)),
                1,
                2,
            ),
            "\x1b[7m好\x1b[m"
        );
        assert_eq!(
            render_text_with_selection(
                "a好b".as_bytes(),
                0,
                Some((&(1..4), SelectionKind::Character)),
                2,
                2,
            ),
            "\x1b[7m \x1b[mb"
        );
    }

    #[test]
    fn line_selection_highlights_line_end_without_extending_character_selection() {
        assert_eq!(
            render_text_with_selection(b"one", 0, Some((&(0..3), SelectionKind::Character)), 0, 10,),
            "\x1b[7mone\x1b[m"
        );
        assert_eq!(
            render_text_with_selection(b"one", 0, Some((&(0..3), SelectionKind::Line)), 0, 10,),
            "\x1b[7mone \x1b[m"
        );
        assert_eq!(
            render_text_with_selection(b"two", 4, Some((&(0..4), SelectionKind::Line)), 0, 10,),
            "two"
        );
        assert_eq!(
            render_text_with_selection(b"", 4, Some((&(4..4), SelectionKind::Line)), 0, 10,),
            "\x1b[7m \x1b[m"
        );
    }

    #[test]
    fn cursor_shape_distinguishes_insert_mode() {
        let mut app = app_with(b"");
        let size = TerminalSize {
            rows: 6,
            columns: 20,
        };

        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.ends_with("\x1b[2 q\x1b[?25h"));

        app.handle_key(Key::Char('i')).unwrap();
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.ends_with("\x1b[6 q\x1b[?25h"));

        app.handle_key(Key::Escape).unwrap();
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.ends_with("\x1b[2 q\x1b[?25h"));
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
    fn rendered_text_replaces_terminal_control_characters() {
        assert_eq!(render_text(b"safe\x1b[31m\n", 0, 20), "safe�[31m�");
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
        assert!(frame.ends_with(":\x1b[6;2H\x1b[2 q\x1b[?25h"));

        app.handle_key(Key::Char('w')).unwrap();
        let frame = String::from_utf8(app.render(size)).unwrap();
        assert!(frame.ends_with(":w\x1b[6;3H\x1b[2 q\x1b[?25h"));
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

        assert!(frame.ends_with("ite\x1b[6;4H\x1b[2 q\x1b[?25h"));
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

        assert!(frame.ends_with("\x1b[2;5H\x1b[2 q\x1b[?25h"));
    }

    #[test]
    fn projects_terminal_scalar_cells_onto_rendered_grapheme_columns() {
        let mut terminal = TerminalEmulator::new(2, 20, 0);
        terminal.process("a👩🏽‍💻x".as_bytes());
        let row = terminal.screen().row(0).unwrap();

        assert_eq!(terminal.screen().cursor().column, 8);
        assert_eq!(terminal_display_column(row, 8), 4);
        assert_eq!(terminal_display_column(row, 2), 2);

        let after_text = TerminalPosition { row: 0, column: 8 };
        let on_x = move_terminal_position(terminal.screen(), after_text, 0, -1);
        let on_emoji = move_terminal_position(terminal.screen(), on_x, 0, -1);
        assert_eq!(on_x.column, 7);
        assert_eq!(on_emoji.column, 1);
        assert_eq!(
            terminal_selection_text(
                terminal.screen(),
                TerminalSelection {
                    anchor: on_emoji,
                    cursor: on_emoji,
                    kind: SelectionKind::Character,
                }
            ),
            "👩🏽‍💻"
        );
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

        assert!(frame.ends_with("/x\x1b[6;3H\x1b[2 q\x1b[?25h"));
    }
}
