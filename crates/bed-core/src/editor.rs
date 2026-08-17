//! Editing operations and history.
//!
//! Cursor movement uses extended grapheme clusters when bytes form valid
//! UTF-8, while malformed input remains editable through a byte-safe fallback.
//! Each buffer owns its bounded undo and redo history, while cursor navigation
//! belongs to the active view.

use crate::{
    Buffer, BufferId, BufferStore, Cursor, DiskReconcile, Document, EditorView, RegexPattern,
    SelectionKind, ViewId,
};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubstituteRange {
    CurrentLine,
    SelectedLines,
    Buffer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineShift {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubstituteOptions {
    pub global: bool,
    pub count_only: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubstituteResult {
    pub matches: usize,
    pub lines: usize,
    pub changed: bool,
}

#[derive(Debug)]
pub struct Editor {
    buffers: BufferStore,
    view_id: ViewId,
    view: EditorView,
    parked_views: HashMap<ViewId, EditorView>,
    primary_views: HashMap<BufferId, ViewId>,
    next_view_id: u64,
    buffer_order: Vec<BufferId>,
}

impl Editor {
    pub fn open(path: PathBuf) -> Result<Self> {
        Ok(Self::new(Document::open(path)?))
    }

    pub fn open_paths(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut paths = paths.into_iter();
        let first = paths.next().context("at least one file path is required")?;
        let mut editor = Self::open(first)?;
        for path in paths {
            editor.open_buffer(path)?;
        }
        editor.switch_buffer_number(1);
        Ok(editor)
    }

    pub fn new(document: Document) -> Self {
        let mut buffers = BufferStore::new();
        let buffer_id = buffers.insert(document);
        let view_id = ViewId(0);
        Self {
            buffers,
            view_id,
            view: EditorView::new(buffer_id),
            parked_views: HashMap::new(),
            primary_views: HashMap::from([(buffer_id, view_id)]),
            next_view_id: 1,
            buffer_order: vec![buffer_id],
        }
    }

    pub fn add_document(&mut self, document: Document) -> BufferId {
        let buffer_id = self.buffers.insert(document);
        let view_id = self.insert_view(EditorView::new(buffer_id));
        self.primary_views.insert(buffer_id, view_id);
        self.buffer_order.push(buffer_id);
        buffer_id
    }

    pub fn open_buffer(&mut self, path: PathBuf) -> Result<BufferId> {
        let buffer_id = if let Some(buffer_id) = self.buffer_id_for_path(&path) {
            buffer_id
        } else {
            self.add_document(Document::open(path)?)
        };
        self.switch_buffer(buffer_id);
        Ok(buffer_id)
    }

    pub fn switch_buffer(&mut self, buffer_id: BufferId) -> bool {
        if buffer_id == self.view.buffer_id {
            return true;
        }
        let Some(&view_id) = self.primary_views.get(&buffer_id) else {
            return false;
        };
        self.switch_view(view_id)
    }

    pub fn switch_view(&mut self, view_id: ViewId) -> bool {
        if view_id == self.view_id {
            self.normalize_active_view_bounds();
            return true;
        }
        let Some(mut next_view) = self.parked_views.remove(&view_id) else {
            return false;
        };
        let document_len = self
            .buffers
            .get(next_view.buffer_id)
            .expect("editor view references a missing buffer")
            .document()
            .len();
        normalize_view_bounds(&mut next_view, document_len);
        let previous_id = self.view_id;
        let previous_view = std::mem::replace(&mut self.view, next_view);
        self.view_id = view_id;
        self.parked_views.insert(previous_id, previous_view);
        true
    }

    fn normalize_active_view_bounds(&mut self) {
        let document_len = self.document().len();
        normalize_view_bounds(&mut self.view, document_len);
    }

    pub fn duplicate_view(&mut self, view_id: ViewId) -> Option<ViewId> {
        let view = if view_id == self.view_id {
            self.view.clone()
        } else {
            self.parked_views.get(&view_id)?.clone()
        };
        Some(self.insert_view(view))
    }

    pub fn remove_view(&mut self, view_id: ViewId) -> bool {
        if view_id == self.view_id || self.primary_views.values().any(|&id| id == view_id) {
            return false;
        }
        self.parked_views.remove(&view_id).is_some()
    }

    pub fn switch_buffer_number(&mut self, number: usize) -> bool {
        let Some(&buffer_id) = number
            .checked_sub(1)
            .and_then(|index| self.buffer_order.get(index))
        else {
            return false;
        };
        self.switch_buffer(buffer_id)
    }

    pub fn next_buffer(&mut self) -> bool {
        self.switch_relative_buffer(1)
    }

    pub fn previous_buffer(&mut self) -> bool {
        self.switch_relative_buffer(-1)
    }

    pub fn close_buffer(&mut self, buffer_id: BufferId) -> bool {
        if self.buffer_order.len() == 1 || !self.buffer_order.contains(&buffer_id) {
            return false;
        }

        if buffer_id == self.view.buffer_id {
            let current = self.buffer_number() - 1;
            let next = self.buffer_order[(current + 1) % self.buffer_order.len()];
            let switched = self.switch_buffer(next);
            debug_assert!(switched);
        }

        self.parked_views
            .retain(|_, view| view.buffer_id != buffer_id);
        self.primary_views.remove(&buffer_id);
        self.buffers.remove(buffer_id);
        self.buffer_order
            .retain(|&candidate| candidate != buffer_id);
        true
    }

    pub fn checkpoint(&mut self) {
        let cursor = self.view.cursor().offset();
        self.buffer_mut().checkpoint(cursor);
    }

    pub fn begin_selection(&mut self) {
        self.view.selection_anchor = Some(self.view.cursor.offset());
    }

    pub fn clear_selection(&mut self) {
        self.view.selection_anchor = None;
    }

    pub fn selection_range(&self, kind: SelectionKind) -> Option<std::ops::Range<usize>> {
        let anchor = self.view.selection_anchor?;
        let cursor = self.view.cursor.offset();
        let first = anchor.min(cursor);
        let last = anchor.max(cursor);
        Some(match kind {
            SelectionKind::Character => {
                first..next_grapheme_offset(self.document().as_bytes(), last)
            }
            SelectionKind::Line => {
                let start = self.document().line_start(first);
                let line_end = self.document().line_end(last);
                start..self.document().line_break_end(line_end)
            }
        })
    }

    pub fn selected_bytes(&self, kind: SelectionKind) -> Option<Vec<u8>> {
        let mut range = self.selection_range(kind)?;
        if kind == SelectionKind::Line {
            let cursor = self.view.cursor.offset();
            let anchor = self.view.selection_anchor?;
            range.end = self.document().line_end(cursor.max(anchor));
        }
        Some(self.document().as_bytes()[range].to_vec())
    }

    pub fn finish_selection(&mut self, kind: SelectionKind) {
        if let Some(range) = self.selection_range(kind) {
            self.view
                .cursor
                .set_offset(range.start.min(self.document().len()));
        }
        self.clear_selection();
        self.normalize_normal_cursor();
    }

    pub fn delete_selection(&mut self, kind: SelectionKind) -> Option<Vec<u8>> {
        let mut range = self.selection_range(kind)?;
        if kind == SelectionKind::Line
            && range.start > 0
            && range.end
                == self.document().line_end(
                    self.view.cursor.offset().max(
                        self.view
                            .selection_anchor
                            .expect("selection range requires an anchor"),
                    ),
                )
        {
            range.start = self.document().preceding_line_break_start(range.start);
        }
        self.clear_selection();
        if range.start >= range.end {
            return None;
        }
        let start = range.start;
        let deleted = self.document_mut().delete_range(range)?;
        self.view
            .cursor
            .set_offset(start.min(self.document().len()));
        if kind == SelectionKind::Line {
            self.move_line_start();
        } else {
            self.normalize_normal_cursor();
        }
        Some(deleted)
    }

    pub fn undo(&mut self) -> bool {
        let cursor = self.view.cursor().offset();
        let Some(cursor) = self.buffer_mut().undo(cursor) else {
            return false;
        };
        self.view.cursor_mut().set_offset(cursor);
        self.view.set_preferred_column(None);
        true
    }

    pub fn redo(&mut self) -> bool {
        let cursor = self.view.cursor().offset();
        let Some(cursor) = self.buffer_mut().redo(cursor) else {
            return false;
        };
        self.view.cursor_mut().set_offset(cursor);
        self.view.set_preferred_column(None);
        true
    }

    pub fn shift_current_lines(&mut self, count: usize, direction: LineShift) -> bool {
        let first_row = self.position().0;
        self.shift_line_range(first_row, count.max(1), direction)
    }

    pub fn shift_selected_lines(&mut self, direction: LineShift) -> bool {
        let Some(anchor) = self.view.selection_anchor else {
            return false;
        };
        let anchor_row = self.document().row_for_offset(anchor);
        let cursor_row = self.document().row_for_offset(self.view.cursor.offset());
        let first_row = anchor_row.min(cursor_row);
        let count = anchor_row.abs_diff(cursor_row) + 1;
        self.shift_line_range(first_row, count, direction)
    }

    pub fn insert(&mut self, byte: u8) -> Result<()> {
        let offset = self.view.cursor.offset();
        self.document_mut().insert(offset, byte)?;
        self.view.cursor.move_right(self.document().len());
        self.view.preferred_column = None;
        Ok(())
    }

    pub fn insert_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let offset = self.view.cursor.offset();
        self.document_mut().insert_bytes(offset, bytes)?;
        self.view.cursor.set_offset(offset + bytes.len());
        self.view.preferred_column = None;
        Ok(())
    }

    pub fn delete(&mut self) -> Option<u8> {
        self.view.preferred_column = None;
        let offset = self.view.cursor.offset();
        self.document_mut().delete(offset)
    }

    pub fn delete_forward(&mut self) -> bool {
        let offset = self.view.cursor.offset();
        if offset >= self.document().len() {
            return false;
        }
        let line_end = self.document().line_end(offset);
        let next = if offset == line_end {
            self.document().line_break_end(line_end)
        } else {
            next_grapheme_offset(self.document().as_bytes(), offset).min(line_end)
        };
        if next == offset {
            return false;
        }
        self.view.preferred_column = None;
        self.document_mut().delete_range(offset..next).is_some()
    }

    pub fn delete_char(&mut self) -> Option<Vec<u8>> {
        let offset = self.view.cursor.offset();
        let line_end = self.document().line_end(offset);
        if offset >= line_end {
            return None;
        }
        let next = next_grapheme_offset(self.document().as_bytes(), offset).min(line_end);
        let deleted = self.document_mut().delete_range(offset..next)?;

        let line_start = self.document().line_start(offset);
        let line_end = self.document().line_end(offset);
        if line_start < line_end && offset == line_end {
            self.view
                .cursor
                .set_offset(previous_grapheme_offset(self.document().as_bytes(), offset));
        }
        self.view.preferred_column = None;
        Some(deleted)
    }

    pub fn can_delete_char(&self) -> bool {
        let offset = self.view.cursor.offset();
        offset < self.document().line_end(offset)
    }

    pub fn delete_line(&mut self) -> Option<Vec<u8>> {
        if self.document().is_empty() {
            return None;
        }

        let offset = self.view.cursor.offset();
        let start = self.document().line_start(offset);
        let end = self.document().line_end(offset);
        // Prefer deleting the following newline. For the final line, consume
        // its preceding newline instead so no empty trailing line is left.
        let break_end = self.document().line_break_end(end);
        let (start, end) = if break_end > end {
            (start, break_end)
        } else if start > 0 {
            (self.document().preceding_line_break_start(start), end)
        } else {
            (start, end)
        };
        if start == end {
            return None;
        }

        let deleted = self.document_mut().delete_range(start..end)?;
        self.view
            .cursor
            .set_offset(start.min(self.document().len()));
        self.move_line_start();
        Some(deleted)
    }

    pub fn backspace(&mut self) -> Option<Vec<u8>> {
        let offset = self.view.cursor.offset();
        if offset == 0 {
            return None;
        }
        let line_start = self.document().line_start(offset);
        let previous = if offset == line_start {
            self.document().preceding_line_break_start(line_start)
        } else {
            previous_grapheme_offset(self.document().as_bytes(), offset)
        };
        self.view.cursor.set_offset(previous);
        self.view.preferred_column = None;
        self.document_mut().delete_range(previous..offset)
    }

    pub fn move_left(&mut self) -> bool {
        let offset = self.view.cursor.offset();
        if offset <= self.document().line_start(offset) {
            return false;
        }
        self.view
            .cursor
            .set_offset(previous_grapheme_offset(self.document().as_bytes(), offset));
        self.view.preferred_column = None;
        true
    }

    pub fn move_right(&mut self, allow_line_end: bool) -> bool {
        let offset = self.view.cursor.offset();
        let line_end = self.document().line_end(offset);
        if offset >= line_end {
            return false;
        }
        let next = next_grapheme_offset(self.document().as_bytes(), offset);
        if !allow_line_end && next >= line_end {
            return false;
        }
        self.view.cursor.set_offset(next);
        self.view.preferred_column = None;
        true
    }

    pub fn move_word_forward(&mut self) -> bool {
        let original = self.view.cursor.offset();
        let target = word_forward_offset(self.document().as_bytes(), original);
        self.view.cursor.set_offset(target);
        self.normalize_normal_cursor();
        self.view.cursor.offset() != original
    }

    pub fn move_word_backward(&mut self) -> bool {
        let original = self.view.cursor.offset();
        let target = word_backward_offset(self.document().as_bytes(), original);
        self.view.cursor.set_offset(target);
        self.view.preferred_column = None;
        target != original
    }

    pub fn move_word_end(&mut self) -> bool {
        let original = self.view.cursor.offset();
        let target = word_end_offset(self.document().as_bytes(), original);
        self.view.cursor.set_offset(target);
        self.normalize_normal_cursor();
        self.view.cursor.offset() != original
    }

    pub fn delete_to_word_forward(&mut self) -> Option<Vec<u8>> {
        let start = self.view.cursor.offset();
        let end = word_forward_offset(self.document().as_bytes(), start)
            .min(self.document().line_end(start));
        self.delete_motion_range(start, end)
    }

    pub fn delete_to_line_end(&mut self) -> Option<Vec<u8>> {
        let start = self.view.cursor.offset();
        let end = self.document().line_end(start);
        self.delete_motion_range(start, end)
    }

    pub fn bytes_to_word_forward(&self) -> Option<Vec<u8>> {
        let start = self.view.cursor.offset();
        let end = word_forward_offset(self.document().as_bytes(), start)
            .min(self.document().line_end(start));
        (start < end).then(|| self.document().as_bytes()[start..end].to_vec())
    }

    pub fn bytes_to_line_end(&self) -> Option<Vec<u8>> {
        let start = self.view.cursor.offset();
        let end = self.document().line_end(start);
        (start < end).then(|| self.document().as_bytes()[start..end].to_vec())
    }

    pub fn current_line(&self) -> &[u8] {
        let start = self.document().line_start(self.view.cursor.offset());
        let end = self.document().line_end(self.view.cursor.offset());
        &self.document().as_bytes()[start..end]
    }

    pub fn put_before(&mut self, bytes: &[u8]) -> Result<bool> {
        if bytes.is_empty() {
            return Ok(false);
        }
        let insertion = self.view.cursor.offset();
        self.document_mut().insert_bytes(insertion, bytes)?;
        self.view.cursor.set_offset(insertion);
        self.view.preferred_column = None;
        Ok(true)
    }

    pub fn put_after(&mut self, bytes: &[u8]) -> Result<bool> {
        if bytes.is_empty() {
            return Ok(false);
        }
        let offset = self.view.cursor.offset();
        let line_end = self.document().line_end(offset);
        let insertion = if offset < line_end {
            next_grapheme_offset(self.document().as_bytes(), offset).min(line_end)
        } else {
            offset
        };
        self.document_mut().insert_bytes(insertion, bytes)?;
        self.view.cursor.set_offset(insertion);
        self.view.preferred_column = None;
        Ok(true)
    }

    pub fn put_line_above(&mut self, bytes: &[u8]) -> Result<()> {
        let insertion = self.document().line_start(self.view.cursor.offset());
        let mut line = Vec::with_capacity(bytes.len() + self.document().line_ending().len());
        line.extend_from_slice(bytes);
        line.extend_from_slice(self.document().line_ending());
        self.document_mut().insert_bytes(insertion, &line)?;
        self.view.cursor.set_offset(insertion);
        self.view.preferred_column = None;
        Ok(())
    }

    pub fn put_line_below(&mut self, bytes: &[u8]) -> Result<()> {
        if self.document().is_empty() {
            let line_ending = self.document().line_ending();
            let mut line = Vec::with_capacity(bytes.len() + line_ending.len());
            line.extend_from_slice(line_ending);
            line.extend_from_slice(bytes);
            self.document_mut().insert_bytes(0, &line)?;
            self.view.cursor.set_offset(line_ending.len());
            self.view.preferred_column = None;
            return Ok(());
        }

        let end = self.document().line_end(self.view.cursor.offset());
        let break_end = self.document().line_break_end(end);
        let line_ending = self.document().line_ending();
        let (insertion, cursor, line) = if break_end > end {
            let mut line = Vec::with_capacity(bytes.len() + line_ending.len());
            line.extend_from_slice(bytes);
            line.extend_from_slice(line_ending);
            (break_end, break_end, line)
        } else {
            let mut line = Vec::with_capacity(bytes.len() + line_ending.len());
            line.extend_from_slice(line_ending);
            line.extend_from_slice(bytes);
            (end, end + line_ending.len(), line)
        };
        self.document_mut().insert_bytes(insertion, &line)?;
        self.view.cursor.set_offset(cursor);
        self.view.preferred_column = None;
        Ok(())
    }

    pub fn search_forward(&mut self, pattern: &RegexPattern) -> bool {
        let offsets = pattern.matching_offsets(self.document().as_bytes());
        let cursor = self.view.cursor.offset();
        let Some(offset) = offsets
            .iter()
            .copied()
            .find(|&offset| offset > cursor)
            .or_else(|| offsets.first().copied())
        else {
            return false;
        };
        self.view.cursor.set_offset(offset);
        self.normalize_normal_cursor();
        true
    }

    pub fn search_backward(&mut self, pattern: &RegexPattern) -> bool {
        let offsets = pattern.matching_offsets(self.document().as_bytes());
        let cursor = self.view.cursor.offset();
        let Some(offset) = offsets
            .iter()
            .rev()
            .copied()
            .find(|&offset| offset < cursor)
            .or_else(|| offsets.last().copied())
        else {
            return false;
        };
        self.view.cursor.set_offset(offset);
        self.normalize_normal_cursor();
        true
    }

    pub fn substitute(
        &mut self,
        range: SubstituteRange,
        pattern: &RegexPattern,
        replacement: &str,
        options: SubstituteOptions,
    ) -> Result<SubstituteResult> {
        let range = self.substitute_byte_range(range);
        let source = self.document().as_bytes()[range.clone()].to_vec();
        let mut output = Vec::with_capacity(source.len());
        let mut matches = 0usize;
        let mut lines = 0usize;
        let mut last_matched_line = None;
        let mut line_start = 0usize;

        loop {
            let newline = source[line_start..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|offset| line_start + offset);
            let separator_end = newline.map_or(source.len(), |offset| offset + 1);
            let content_end = newline.map_or(source.len(), |offset| {
                if offset > line_start && source[offset - 1] == b'\r' {
                    offset - 1
                } else {
                    offset
                }
            });
            let content = &source[line_start..content_end];
            let output_line_start = output.len();
            let mut copied = 0usize;
            let mut line_matches = 0usize;
            let mut previous_match_end = None;

            for captures in pattern.captures_iter(content) {
                let matched = captures
                    .get(0)
                    .expect("regular expression captures include the complete match");
                if matched.is_empty() && previous_match_end == Some(matched.start()) {
                    continue;
                }
                output.extend_from_slice(&content[copied..matched.start()]);
                captures.expand(replacement.as_bytes(), &mut output);
                copied = matched.end();
                previous_match_end = Some(matched.end());
                matches = matches.saturating_add(1);
                line_matches = line_matches.saturating_add(1);
                if !options.global {
                    break;
                }
            }
            output.extend_from_slice(&content[copied..]);
            output.extend_from_slice(&source[content_end..separator_end]);
            if line_matches > 0 {
                lines = lines.saturating_add(1);
                last_matched_line = Some(output_line_start);
            }

            if separator_end == source.len() {
                break;
            }
            line_start = separator_end;
        }

        if matches == 0 || options.count_only {
            return Ok(SubstituteResult {
                matches,
                lines,
                changed: false,
            });
        }

        let changed = output != source;
        if changed {
            self.checkpoint();
            if !range.is_empty() {
                self.document_mut()
                    .delete_range(range.clone())
                    .expect("validated substitute range must be deletable");
            }
            self.document_mut().insert_bytes(range.start, &output)?;
        }
        if let Some(last_matched_line) = last_matched_line {
            self.view
                .cursor
                .set_offset((range.start + last_matched_line).min(self.document().len()));
            self.move_line_start();
        }
        Ok(SubstituteResult {
            matches,
            lines,
            changed,
        })
    }

    pub fn move_up(&mut self, allow_line_end: bool) -> bool {
        self.move_vertical(-1, allow_line_end)
    }

    pub fn move_down(&mut self, allow_line_end: bool) -> bool {
        self.move_vertical(1, allow_line_end)
    }

    pub fn move_line_start(&mut self) {
        self.view
            .cursor
            .set_offset(self.document().line_start(self.view.cursor.offset()));
        self.view.preferred_column = None;
    }

    pub fn move_line_end(&mut self, allow_line_end: bool) {
        let start = self.document().line_start(self.view.cursor.offset());
        let end = self.document().line_end(self.view.cursor.offset());
        let offset = if allow_line_end || start == end {
            end
        } else {
            previous_grapheme_offset(self.document().as_bytes(), end)
        };
        self.view.cursor.set_offset(offset);
        self.view.preferred_column = None;
    }

    pub fn normalize_normal_cursor(&mut self) {
        let offset = self.view.cursor.offset();
        let start = self.document().line_start(offset);
        let end = self.document().line_end(offset);
        if start < end && offset >= end {
            self.view
                .cursor
                .set_offset(previous_grapheme_offset(self.document().as_bytes(), end));
        }
        self.view.preferred_column = None;
    }

    pub fn move_to_first_line(&mut self) {
        self.view.cursor.set_offset(0);
        self.view.preferred_column = None;
    }

    /// Move to a 1-based document line, clamping outside values to the file.
    pub fn move_to_line(&mut self, line: usize) {
        let row = line
            .saturating_sub(1)
            .min(self.document().line_count().saturating_sub(1));
        let offset = self.document().line_start_by_row(row).unwrap_or(0);
        self.view.cursor.set_offset(offset);
        self.view.preferred_column = None;
    }

    pub fn move_to_last_line(&mut self) {
        let start = self.document().line_start(self.document().len());
        self.view.cursor.set_offset(start);
        self.view.preferred_column = None;
    }

    pub fn open_line_below(&mut self) -> Result<()> {
        let end = self.document().line_end(self.view.cursor.offset());
        let break_end = self.document().line_break_end(end);
        let insertion = if break_end > end { break_end } else { end };
        let line_ending = self.document().line_ending();
        self.document_mut().insert_bytes(insertion, line_ending)?;
        self.view.cursor.set_offset(if break_end > end {
            insertion
        } else {
            insertion + line_ending.len()
        });
        self.view.preferred_column = None;
        Ok(())
    }

    pub fn open_line_above(&mut self) -> Result<()> {
        let start = self.document().line_start(self.view.cursor.offset());
        let line_ending = self.document().line_ending();
        self.document_mut().insert_bytes(start, line_ending)?;
        self.view.cursor.set_offset(start);
        self.view.preferred_column = None;
        Ok(())
    }

    pub fn insert_newline(&mut self) -> Result<()> {
        let line_ending = self.document().line_ending();
        self.insert_bytes(line_ending)
    }

    pub fn insert_paste(&mut self, text: &str) -> Result<()> {
        let line_ending = self.document().line_ending();
        let mut bytes = Vec::with_capacity(text.len());
        let source = text.as_bytes();
        let mut offset = 0;
        while offset < source.len() {
            match source[offset] {
                b'\r' if source.get(offset + 1) == Some(&b'\n') => {
                    bytes.extend_from_slice(line_ending);
                    offset += 2;
                }
                b'\r' | b'\n' => {
                    bytes.extend_from_slice(line_ending);
                    offset += 1;
                }
                _ => {
                    bytes.push(source[offset]);
                    offset += 1;
                }
            }
        }
        self.insert_bytes(&bytes)
    }

    pub fn save(&mut self) -> Result<()> {
        self.document_mut().save()
    }

    pub fn save_force(&mut self) -> Result<()> {
        self.document_mut().save_force()
    }

    pub fn save_all(&mut self) -> Result<usize> {
        self.save_all_impl(false)
    }

    pub fn save_all_force(&mut self) -> Result<usize> {
        self.save_all_impl(true)
    }

    pub fn reconcile_buffer_disk(&mut self, buffer_id: BufferId) -> Result<DiskReconcile> {
        let buffer = self
            .buffers
            .get_mut(buffer_id)
            .context("buffer disappeared while reconciling its file")?;
        let result = buffer.document_mut().reconcile_disk()?;
        let document_len = buffer.document().len();

        if result != DiskReconcile::Unchanged {
            if self.view.buffer_id == buffer_id {
                normalize_view_bounds(&mut self.view, document_len);
            }
            for view in self.parked_views.values_mut() {
                if view.buffer_id == buffer_id {
                    normalize_view_bounds(view, document_len);
                }
            }
        }
        Ok(result)
    }

    fn save_all_impl(&mut self, force: bool) -> Result<usize> {
        let mut written = 0;
        for &buffer_id in &self.buffer_order {
            let buffer = self
                .buffers
                .get_mut(buffer_id)
                .expect("buffer order references a missing buffer");
            if buffer.document().has_unsaved_changes() {
                if force {
                    buffer.document_mut().save_force()?;
                } else {
                    buffer.document_mut().save()?;
                }
                written += 1;
            }
        }
        Ok(written)
    }

    pub fn document(&self) -> &Document {
        self.buffer().document()
    }

    pub fn cursor(&self) -> Cursor {
        self.view.cursor
    }

    pub fn buffer_id(&self) -> BufferId {
        self.view.buffer_id()
    }

    pub fn buffer_ids(&self) -> &[BufferId] {
        &self.buffer_order
    }

    pub fn buffer_count(&self) -> usize {
        self.buffer_order.len()
    }

    pub fn buffer_id_at(&self, number: usize) -> Option<BufferId> {
        number
            .checked_sub(1)
            .and_then(|index| self.buffer_order.get(index))
            .copied()
    }

    pub fn buffer_number(&self) -> usize {
        self.buffer_order
            .iter()
            .position(|&buffer_id| buffer_id == self.view.buffer_id)
            .expect("active buffer is missing from the buffer order")
            + 1
    }

    pub fn has_dirty_buffers(&self) -> bool {
        self.buffer_order.iter().any(|&buffer_id| {
            self.buffers
                .get(buffer_id)
                .is_some_and(|buffer| buffer.document().has_unsaved_changes())
        })
    }

    pub fn dirty_buffer_count(&self) -> usize {
        self.buffer_order
            .iter()
            .filter(|&&buffer_id| {
                self.buffers
                    .get(buffer_id)
                    .is_some_and(|buffer| buffer.document().has_unsaved_changes())
            })
            .count()
    }

    pub fn buffers(&self) -> &BufferStore {
        &self.buffers
    }

    pub fn view(&self) -> &EditorView {
        &self.view
    }

    pub fn view_by_id(&self, view_id: ViewId) -> Option<&EditorView> {
        if view_id == self.view_id {
            Some(&self.view)
        } else {
            self.parked_views.get(&view_id)
        }
    }

    pub fn view_id(&self) -> ViewId {
        self.view_id
    }

    pub fn view_for_buffer(&self, buffer_id: BufferId) -> Option<ViewId> {
        self.primary_views.get(&buffer_id).copied()
    }

    pub fn current_line_prefix(&self) -> &[u8] {
        let offset = self.view.cursor.offset();
        let start = self.document().line_start(offset);
        &self.document().as_bytes()[start..offset]
    }

    pub fn position(&self) -> (usize, usize) {
        let offset = self.view.cursor.offset();
        let row = self.document().row_for_offset(offset);
        let start = self.document().line_start(offset);
        let column = grapheme_count(&self.document().as_bytes()[start..offset]);
        (row, column)
    }

    pub fn line_byte_range(&self, row: usize) -> Option<std::ops::Range<usize>> {
        let start = self.document().line_start_by_row(row)?;
        Some(start..self.document().line_end(start))
    }

    fn move_vertical(&mut self, delta: isize, allow_line_end: bool) -> bool {
        let (row, current_column) = self.position();
        let column = self.view.preferred_column.unwrap_or(current_column);
        let target_row = if delta < 0 {
            let Some(row) = row.checked_sub(delta.unsigned_abs()) else {
                return false;
            };
            row
        } else {
            row + delta as usize
        };
        let Some(start) = self.document().line_start_by_row(target_row) else {
            return false;
        };
        let end = self.document().line_end(start);
        let mut offset = start;
        for _ in 0..column {
            if offset >= end {
                break;
            }
            offset = next_grapheme_offset(self.document().as_bytes(), offset);
        }
        if !allow_line_end && start < end && offset == end {
            offset = previous_grapheme_offset(self.document().as_bytes(), end);
        }
        self.view.cursor.set_offset(offset);
        self.view.preferred_column = Some(column);
        true
    }

    fn buffer_id_for_path(&self, path: &Path) -> Option<BufferId> {
        let requested = path_identity(path);
        self.buffer_order.iter().copied().find(|&buffer_id| {
            self.buffers.get(buffer_id).is_some_and(|buffer| {
                buffer.document().is_file_backed()
                    && path_identity(buffer.document().path()) == requested
            })
        })
    }

    fn substitute_byte_range(&self, range: SubstituteRange) -> std::ops::Range<usize> {
        match range {
            SubstituteRange::CurrentLine => {
                let start = self.document().line_start(self.view.cursor.offset());
                let end = self.document().line_end(self.view.cursor.offset());
                start..self.document().line_break_end(end)
            }
            SubstituteRange::SelectedLines => {
                let anchor = self
                    .view
                    .selection_anchor
                    .unwrap_or(self.view.cursor.offset());
                let first = anchor.min(self.view.cursor.offset());
                let last = anchor.max(self.view.cursor.offset());
                let start = self.document().line_start(first);
                let end = self.document().line_end(last);
                start..self.document().line_break_end(end)
            }
            SubstituteRange::Buffer => 0..self.document().len(),
        }
    }

    fn switch_relative_buffer(&mut self, delta: isize) -> bool {
        if self.buffer_order.len() < 2 {
            return false;
        }
        let current = self.buffer_number() - 1;
        let next = if delta < 0 {
            current
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(self.buffer_order.len() - 1)
        } else {
            (current + delta as usize) % self.buffer_order.len()
        };
        let buffer_id = self.buffer_order[next];
        self.switch_buffer(buffer_id)
    }

    fn insert_view(&mut self, view: EditorView) -> ViewId {
        let view_id = ViewId(self.next_view_id);
        self.next_view_id = self
            .next_view_id
            .checked_add(1)
            .expect("view ID space exhausted");
        let previous = self.parked_views.insert(view_id, view);
        debug_assert!(previous.is_none());
        view_id
    }

    fn buffer(&self) -> &Buffer {
        self.buffers
            .get(self.view.buffer_id)
            .expect("editor view references a missing buffer")
    }

    fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffers
            .get_mut(self.view.buffer_id)
            .expect("editor view references a missing buffer")
    }

    fn document_mut(&mut self) -> &mut Document {
        self.buffer_mut().document_mut()
    }

    fn delete_motion_range(&mut self, start: usize, end: usize) -> Option<Vec<u8>> {
        if start >= end {
            return None;
        }
        let deleted = self.document_mut().delete_range(start..end)?;
        self.view
            .cursor
            .set_offset(start.min(self.document().len()));
        self.normalize_normal_cursor();
        Some(deleted)
    }

    fn shift_line_range(&mut self, first_row: usize, count: usize, direction: LineShift) -> bool {
        const INDENT: &[u8] = b"    ";

        let line_count = self.document().line_count();
        if first_row >= line_count {
            return false;
        }
        let end_row = first_row
            .saturating_add(count)
            .min(line_count)
            .max(first_row + 1);
        let mut edits = Vec::with_capacity(end_row - first_row);
        for row in first_row..end_row {
            let start = self
                .document()
                .line_start_by_row(row)
                .expect("bounded row must have a line start");
            let end = self.document().line_end(start);
            match direction {
                LineShift::Right if start < end => edits.push((start, 0usize, INDENT.len())),
                LineShift::Right => {}
                LineShift::Left => {
                    let line = &self.document().as_bytes()[start..end];
                    let removed = if line.first() == Some(&b'\t') {
                        1
                    } else {
                        line.iter()
                            .take(INDENT.len())
                            .take_while(|&&byte| byte == b' ')
                            .count()
                    };
                    if removed > 0 {
                        edits.push((start, removed, 0));
                    }
                }
            }
        }
        if edits.is_empty() {
            return false;
        }

        self.checkpoint();
        let mut cursor = self.view.cursor.offset();
        let mut anchor = self.view.selection_anchor;
        for &(start, removed, inserted) in edits.iter().rev() {
            if removed > 0 {
                let end = start + removed;
                self.document_mut()
                    .delete_range(start..end)
                    .expect("validated indentation must be deletable");
                cursor = offset_after_deletion(cursor, start, end);
                anchor = anchor.map(|offset| offset_after_deletion(offset, start, end));
            } else {
                self.document_mut()
                    .insert_bytes(start, INDENT)
                    .expect("line starts are valid insertion offsets");
                cursor = offset_after_insertion(cursor, start, inserted);
                anchor = anchor.map(|offset| offset_after_insertion(offset, start, inserted));
            }
        }
        self.view.cursor.set_offset(cursor);
        self.view.selection_anchor = anchor;
        self.view.preferred_column = None;
        true
    }
}

fn offset_after_insertion(offset: usize, start: usize, inserted: usize) -> usize {
    if offset >= start {
        offset.saturating_add(inserted)
    } else {
        offset
    }
}

fn offset_after_deletion(offset: usize, start: usize, end: usize) -> usize {
    if offset >= end {
        offset - (end - start)
    } else if offset > start {
        start
    } else {
        offset
    }
}

fn next_grapheme_offset(bytes: &[u8], offset: usize) -> usize {
    let tail = &bytes[offset.min(bytes.len())..];
    let valid_length = std::str::from_utf8(tail).map_or_else(|error| error.valid_up_to(), str::len);
    if valid_length > 0 {
        let text = std::str::from_utf8(&tail[..valid_length]).expect("validated UTF-8 prefix");
        if let Some(grapheme) = text.graphemes(true).next() {
            return offset + grapheme.len();
        }
    }

    // Invalid UTF-8 must not trap the cursor. Skip one invalid leading byte and
    // any following continuation bytes to reach the next plausible boundary.
    let mut next = (offset + 1).min(bytes.len());
    while next < bytes.len() && bytes[next] & 0b1100_0000 == 0b1000_0000 {
        next += 1;
    }
    next
}

fn previous_grapheme_offset(bytes: &[u8], offset: usize) -> usize {
    let prefix = &bytes[..offset.min(bytes.len())];
    if let Ok(text) = std::str::from_utf8(prefix)
        && let Some((index, _)) = text.grapheme_indices(true).next_back()
    {
        return index;
    }

    // Mirror the forward fallback by backing over UTF-8 continuation bytes.
    let mut previous = offset.saturating_sub(1);
    while previous > 0 && bytes[previous] & 0b1100_0000 == 0b1000_0000 {
        previous -= 1;
    }
    previous
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Whitespace,
    Keyword,
    Punctuation,
}

fn word_class(bytes: &[u8], offset: usize) -> WordClass {
    let end = next_grapheme_offset(bytes, offset);
    let Ok(grapheme) = std::str::from_utf8(&bytes[offset..end]) else {
        return WordClass::Punctuation;
    };
    if grapheme.chars().all(char::is_whitespace) {
        WordClass::Whitespace
    } else if grapheme
        .chars()
        .next()
        .is_some_and(|character| character.is_alphanumeric() || character == '_')
    {
        WordClass::Keyword
    } else {
        WordClass::Punctuation
    }
}

fn word_forward_offset(bytes: &[u8], offset: usize) -> usize {
    if offset >= bytes.len() {
        return bytes.len();
    }

    let mut position = offset;
    let class = word_class(bytes, position);
    while position < bytes.len() && word_class(bytes, position) == class {
        position = next_grapheme_offset(bytes, position);
    }
    if class != WordClass::Whitespace {
        while position < bytes.len() && word_class(bytes, position) == WordClass::Whitespace {
            position = next_grapheme_offset(bytes, position);
        }
    }
    position
}

fn word_backward_offset(bytes: &[u8], offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }

    let mut position = previous_grapheme_offset(bytes, offset);
    while position > 0 && word_class(bytes, position) == WordClass::Whitespace {
        position = previous_grapheme_offset(bytes, position);
    }
    let class = word_class(bytes, position);
    while position > 0 {
        let previous = previous_grapheme_offset(bytes, position);
        if word_class(bytes, previous) != class {
            break;
        }
        position = previous;
    }
    position
}

fn word_end_offset(bytes: &[u8], offset: usize) -> usize {
    if offset >= bytes.len() {
        return bytes.len();
    }

    let current_class = word_class(bytes, offset);
    let mut position = offset;
    let mut next = next_grapheme_offset(bytes, position);
    if current_class != WordClass::Whitespace
        && next < bytes.len()
        && word_class(bytes, next) == current_class
    {
        while next < bytes.len() && word_class(bytes, next) == current_class {
            position = next;
            next = next_grapheme_offset(bytes, next);
        }
        return position;
    }

    position = next;
    while position < bytes.len() && word_class(bytes, position) == WordClass::Whitespace {
        position = next_grapheme_offset(bytes, position);
    }
    if position >= bytes.len() {
        return offset;
    }

    let class = word_class(bytes, position);
    next = next_grapheme_offset(bytes, position);
    while next < bytes.len() && word_class(bytes, next) == class {
        position = next;
        next = next_grapheme_offset(bytes, next);
    }
    position
}

fn grapheme_count(bytes: &[u8]) -> usize {
    String::from_utf8_lossy(bytes).graphemes(true).count()
}

fn normalize_view_bounds(view: &mut EditorView, document_len: usize) {
    if view.cursor.offset() > document_len {
        view.cursor.set_offset(document_len);
        view.preferred_column = None;
    }
    if let Some(anchor) = view.selection_anchor {
        view.selection_anchor = Some(anchor.min(document_len));
    }
}

fn path_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| lexical_absolute(path))
}

fn lexical_absolute(path: &Path) -> PathBuf {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::{Editor, LineShift, SubstituteOptions, SubstituteRange};
    use crate::{Document, RegexPattern, SelectionKind};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("bed-editor-paths-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn editor_with(bytes: &[u8]) -> Editor {
        Editor::new(Document::new(PathBuf::from("test.txt"), bytes.to_vec()))
    }

    #[test]
    fn inserts_at_the_cursor_and_advances_it() {
        let mut editor = editor_with(b"abc");

        assert!(editor.move_right(true));
        editor.insert(b'X').unwrap();

        assert_eq!(editor.document().as_bytes(), b"aXbc");
        assert_eq!(editor.cursor().offset(), 2);
    }

    #[test]
    fn exposes_the_active_buffer_and_view_without_duplicating_state() {
        let editor = editor_with(b"abc");

        assert_eq!(editor.buffers().len(), 1);
        assert_eq!(editor.buffer_id(), editor.view().buffer_id());
        assert_eq!(
            editor
                .buffers()
                .get(editor.buffer_id())
                .unwrap()
                .document()
                .as_bytes(),
            editor.document().as_bytes()
        );
        assert_eq!(editor.view().cursor(), editor.cursor());
    }

    #[test]
    fn switches_buffers_without_losing_view_or_history_state() {
        let mut editor = editor_with(b"one");
        let first = editor.buffer_id();
        editor.move_right(false);
        let second = editor.add_document(Document::new(PathBuf::from("two"), b"two".to_vec()));

        assert!(editor.switch_buffer(second));
        editor.move_right(false);
        editor.move_right(false);
        editor.checkpoint();
        editor.insert(b'X').unwrap();

        assert!(editor.switch_buffer(first));
        assert_eq!(editor.cursor().offset(), 1);
        editor.checkpoint();
        editor.insert(b'A').unwrap();

        assert!(editor.switch_buffer(second));
        assert_eq!(editor.document().as_bytes(), b"twXo");
        assert_eq!(editor.cursor().offset(), 3);
        assert!(editor.undo());
        assert_eq!(editor.document().as_bytes(), b"two");
        assert_eq!(editor.cursor().offset(), 2);

        assert!(editor.switch_buffer(first));
        assert_eq!(editor.document().as_bytes(), b"oAne");
        assert_eq!(editor.dirty_buffer_count(), 1);
        assert!(editor.has_dirty_buffers());
    }

    #[test]
    fn switches_buffers_by_number_and_wraps_at_the_ends() {
        let mut editor = editor_with(b"one");
        editor.add_document(Document::new(PathBuf::from("two"), b"two".to_vec()));
        editor.add_document(Document::new(PathBuf::from("three"), b"three".to_vec()));

        assert_eq!(editor.buffer_count(), 3);
        assert_eq!(editor.buffer_number(), 1);
        assert!(editor.previous_buffer());
        assert_eq!(editor.buffer_number(), 3);
        assert!(editor.next_buffer());
        assert_eq!(editor.buffer_number(), 1);
        assert!(editor.switch_buffer_number(2));
        assert_eq!(editor.document().path(), PathBuf::from("two"));
        assert!(!editor.switch_buffer_number(0));
        assert!(!editor.switch_buffer_number(4));
    }

    #[test]
    fn reopening_the_same_path_reuses_its_buffer() {
        let mut editor = editor_with(b"one");
        let original = editor.buffer_id();

        assert_eq!(
            editor.open_buffer(PathBuf::from("test.txt")).unwrap(),
            original
        );
        assert_eq!(editor.buffer_count(), 1);
    }

    #[test]
    fn path_aliases_reuse_an_existing_buffer() {
        let directory = TempDir::new();
        let path = directory.0.join("file.txt");
        fs::write(&path, b"one").unwrap();
        let mut editor = Editor::open(path.clone()).unwrap();
        let original = editor.buffer_id();

        assert_eq!(
            editor
                .open_buffer(directory.0.join(".").join("file.txt"))
                .unwrap(),
            original
        );
        assert_eq!(editor.buffer_count(), 1);
    }

    #[test]
    fn missing_path_aliases_reuse_an_existing_buffer() {
        let directory = TempDir::new();
        fs::create_dir(directory.0.join("nested")).unwrap();
        let path = directory.0.join("missing.txt");
        let alias = directory.0.join("nested").join("..").join("missing.txt");
        let mut editor = Editor::open(path).unwrap();
        let original = editor.buffer_id();

        assert_eq!(editor.open_buffer(alias).unwrap(), original);
        assert_eq!(editor.buffer_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_aliases_reuse_an_existing_buffer() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new();
        let path = directory.0.join("file.txt");
        let alias = directory.0.join("alias.txt");
        fs::write(&path, b"one").unwrap();
        symlink(&path, &alias).unwrap();
        let mut editor = Editor::open(path).unwrap();
        let original = editor.buffer_id();

        assert_eq!(editor.open_buffer(alias).unwrap(), original);
        assert_eq!(editor.buffer_count(), 1);
    }

    #[test]
    fn closes_active_and_parked_buffers_without_dangling_views() {
        let mut editor = editor_with(b"one");
        let first = editor.buffer_id();
        let second = editor.add_document(Document::new(PathBuf::from("two"), b"two".to_vec()));
        let third = editor.add_document(Document::new(PathBuf::from("three"), b"three".to_vec()));

        assert!(editor.close_buffer(second));
        assert!(editor.buffers().get(second).is_none());
        assert_eq!(editor.buffer_ids(), &[first, third]);

        assert!(editor.close_buffer(first));
        assert_eq!(editor.buffer_id(), third);
        assert_eq!(editor.buffer_count(), 1);
        assert!(!editor.close_buffer(third));
    }

    #[test]
    fn duplicated_views_have_independent_cursors_on_one_buffer() {
        let mut editor = editor_with(b"abcd");
        let original = editor.view_id();
        editor.move_right(false);
        let duplicate = editor.duplicate_view(original).unwrap();

        assert!(editor.switch_view(duplicate));
        editor.move_right(false);
        assert_eq!(editor.cursor().offset(), 2);

        assert!(editor.switch_view(original));
        assert_eq!(editor.cursor().offset(), 1);
        assert_eq!(editor.view().buffer_id(), editor.buffer_id());

        assert!(editor.remove_view(duplicate));
        assert!(!editor.switch_view(duplicate));
        assert!(!editor.remove_view(original));
    }

    #[test]
    fn switching_views_clamps_a_cursor_after_shared_text_shrinks() {
        let mut editor = editor_with(b"abcdef");
        let original = editor.view_id();
        editor.move_line_end(false);
        let duplicate = editor.duplicate_view(original).unwrap();

        assert!(editor.switch_view(duplicate));
        editor.move_to_first_line();
        editor.begin_selection();
        editor.move_line_end(false);
        editor.checkpoint();
        editor.delete_selection(SelectionKind::Character);
        assert_eq!(editor.document().as_bytes(), b"");

        assert!(editor.switch_view(original));
        assert_eq!(editor.cursor().offset(), 0);
    }

    #[test]
    fn backspace_removes_the_previous_character() {
        let mut editor = editor_with("a好c".as_bytes());

        assert!(editor.move_right(true));
        assert!(editor.move_right(true));
        assert_eq!(editor.backspace(), Some("好".as_bytes().to_vec()));

        assert_eq!(editor.document().as_bytes(), b"ac");
        assert_eq!(editor.cursor().offset(), 1);
    }

    #[test]
    fn moves_vertically_and_keeps_the_preferred_column() {
        let mut editor = editor_with(b"abcd\nx\nwxyz");
        editor.move_right(false);
        editor.move_right(false);
        editor.move_right(false);

        assert!(editor.move_down(false));
        assert_eq!(editor.position(), (1, 0));
        assert!(editor.move_down(false));
        assert_eq!(editor.position(), (2, 3));
    }

    #[test]
    fn deletes_and_undoes_a_line() {
        let mut editor = editor_with(b"one\ntwo\nthree");
        editor.move_down(false);
        editor.checkpoint();

        assert!(editor.delete_line().is_some());
        assert_eq!(editor.document().as_bytes(), b"one\nthree");
        assert!(editor.undo());
        assert_eq!(editor.document().as_bytes(), b"one\ntwo\nthree");
        assert_eq!(editor.position(), (1, 0));
    }

    #[test]
    fn shifts_counted_lines_as_one_undoable_change() {
        let mut editor = editor_with(b"one\n  two\n\tthree\nfour");
        assert!(editor.move_down(false));

        assert!(editor.shift_current_lines(2, LineShift::Right));
        assert_eq!(
            editor.document().as_bytes(),
            b"one\n      two\n    \tthree\nfour"
        );
        assert_eq!(editor.position(), (1, 4));

        assert!(editor.undo());
        assert_eq!(editor.document().as_bytes(), b"one\n  two\n\tthree\nfour");
        assert_eq!(editor.position(), (1, 0));
    }

    #[test]
    fn shifts_selected_lines_and_preserves_the_selection() {
        let mut editor = editor_with(b"one\n  two\nthree");
        editor.begin_selection();
        assert!(editor.move_down(false));

        assert!(editor.shift_selected_lines(LineShift::Right));
        assert_eq!(editor.document().as_bytes(), b"    one\n      two\nthree");
        assert_eq!(editor.position(), (1, 4));
        assert!(editor.view().selection_anchor().is_some());

        assert!(editor.shift_selected_lines(LineShift::Left));
        assert_eq!(editor.document().as_bytes(), b"one\n  two\nthree");
        assert_eq!(editor.position(), (1, 0));
        assert!(editor.view().selection_anchor().is_some());
    }

    #[test]
    fn unindents_tabs_and_spaces_without_clearing_redo_on_a_noop() {
        let mut editor = editor_with(b"    one\n  two\n\tthree\nfour");
        assert!(editor.shift_current_lines(3, LineShift::Left));
        assert_eq!(editor.document().as_bytes(), b"one\ntwo\nthree\nfour");

        editor.checkpoint();
        editor.insert(b'!').unwrap();
        assert!(editor.undo());
        assert!(!editor.shift_current_lines(3, LineShift::Left));
        assert!(editor.redo());
        assert_eq!(editor.document().as_bytes(), b"!one\ntwo\nthree\nfour");
    }

    #[test]
    fn opens_lines_above_and_below() {
        let mut below = editor_with(b"one\ntwo");
        below.open_line_below().unwrap();
        assert_eq!(below.document().as_bytes(), b"one\n\ntwo");
        assert_eq!(below.position(), (1, 0));

        let mut above = editor_with(b"one\ntwo");
        above.move_down(false);
        above.open_line_above().unwrap();
        assert_eq!(above.document().as_bytes(), b"one\n\ntwo");
        assert_eq!(above.position(), (1, 0));
    }

    #[test]
    fn edits_crlf_lines_without_splitting_the_separator() {
        let mut editor = editor_with(b"one\r\ntwo");
        editor.move_line_end(true);
        editor.insert_newline().unwrap();
        assert_eq!(editor.document().as_bytes(), b"one\r\n\r\ntwo");

        assert!(editor.backspace().is_some());
        assert_eq!(editor.document().as_bytes(), b"one\r\ntwo");

        editor.move_line_end(true);
        assert!(editor.delete_forward());
        assert_eq!(editor.document().as_bytes(), b"onetwo");
    }

    #[test]
    fn deleting_the_last_character_keeps_a_normal_cursor_position() {
        let mut editor = editor_with("a好".as_bytes());
        assert!(editor.move_right(false));

        assert!(editor.delete_char().is_some());

        assert_eq!(editor.document().as_bytes(), b"a");
        assert_eq!(editor.cursor().offset(), 0);
    }

    #[test]
    fn normalizes_an_insert_cursor_at_the_end_of_a_line() {
        let mut editor = editor_with(b"ab");
        editor.move_line_end(true);

        editor.normalize_normal_cursor();

        assert_eq!(editor.cursor().offset(), 1);
    }

    #[test]
    fn moves_by_unicode_words_and_punctuation() {
        let mut editor = editor_with("one 你好, three".as_bytes());

        assert!(editor.move_word_forward());
        assert_eq!(editor.current_line_prefix(), b"one ");
        assert!(editor.move_word_end());
        assert_eq!(editor.current_line_prefix(), "one 你".as_bytes());
        assert!(editor.move_word_forward());
        assert_eq!(editor.current_line_prefix(), "one 你好".as_bytes());
        assert!(editor.move_word_backward());
        assert_eq!(editor.current_line_prefix(), "one ".as_bytes());
    }

    #[test]
    fn deletes_word_and_line_end_ranges() {
        let mut editor = editor_with(b"one two three");

        assert_eq!(editor.delete_to_word_forward(), Some(b"one ".to_vec()));
        assert_eq!(editor.document().as_bytes(), b"two three");
        assert_eq!(editor.delete_to_line_end(), Some(b"two three".to_vec()));
        assert!(editor.document().is_empty());

        let mut line_boundary = editor_with(b"one\ntwo");
        assert_eq!(
            line_boundary.delete_to_word_forward(),
            Some(b"one".to_vec())
        );
        assert_eq!(line_boundary.document().as_bytes(), b"\ntwo");
    }

    #[test]
    fn puts_characterwise_and_linewise_text() {
        let mut characterwise = editor_with(b"ac");
        characterwise.put_after(b"b").unwrap();
        assert_eq!(characterwise.document().as_bytes(), b"abc");
        assert_eq!(characterwise.cursor().offset(), 1);

        let mut linewise = editor_with(b"one\ntwo");
        linewise.put_line_below(b"middle").unwrap();
        assert_eq!(linewise.document().as_bytes(), b"one\nmiddle\ntwo");
        assert_eq!(linewise.position(), (1, 0));
        linewise.put_line_above(b"before").unwrap();
        assert_eq!(linewise.document().as_bytes(), b"one\nbefore\nmiddle\ntwo");
    }

    #[test]
    fn searches_by_grapheme_boundary_and_wraps() {
        let mut editor = editor_with("one 你好 one".as_bytes());
        let one = RegexPattern::compile("o.e").unwrap();

        assert!(editor.search_forward(&one));
        assert_eq!(editor.current_line_prefix(), "one 你好 ".as_bytes());
        assert!(editor.search_forward(&one));
        assert_eq!(editor.cursor().offset(), 0);
        assert!(editor.search_backward(&RegexPattern::compile("你.").unwrap()));
        assert_eq!(editor.current_line_prefix(), b"one ");
        assert!(!editor.search_forward(&RegexPattern::compile("missing").unwrap()));
        assert!(RegexPattern::compile("(").is_err());
    }

    #[test]
    fn substitutes_regex_captures_as_one_undoable_change() {
        let mut editor = editor_with(b"a=1 a=2\r\nb=3");
        let pattern = RegexPattern::compile("(?P<name>[a-z])=([0-9])").unwrap();

        let result = editor
            .substitute(
                SubstituteRange::Buffer,
                &pattern,
                "$2:${name}",
                SubstituteOptions {
                    global: true,
                    count_only: false,
                },
            )
            .unwrap();

        assert_eq!(result.matches, 3);
        assert_eq!(result.lines, 2);
        assert!(result.changed);
        assert_eq!(editor.document().as_bytes(), b"1:a 2:a\r\n3:b");
        assert!(editor.undo());
        assert_eq!(editor.document().as_bytes(), b"a=1 a=2\r\nb=3");
    }

    #[test]
    fn substitutes_once_per_line_and_counts_without_editing() {
        let mut editor = editor_with(b"a=a=a\nb=b=b");
        let pattern = RegexPattern::compile("=").unwrap();

        let result = editor
            .substitute(
                SubstituteRange::Buffer,
                &pattern,
                ":",
                SubstituteOptions::default(),
            )
            .unwrap();
        assert_eq!(result.matches, 2);
        assert_eq!(editor.document().as_bytes(), b"a:a=a\nb:b=b");

        let count = editor
            .substitute(
                SubstituteRange::Buffer,
                &RegexPattern::compile("[ab]").unwrap(),
                "ignored",
                SubstituteOptions {
                    global: true,
                    count_only: true,
                },
            )
            .unwrap();
        assert_eq!(count.matches, 6);
        assert!(!count.changed);
        assert_eq!(editor.document().as_bytes(), b"a:a=a\nb:b=b");
    }

    #[test]
    fn substitution_replacement_is_not_vim_compatible() {
        let mut editor = editor_with(b"x=1");
        editor
            .substitute(
                SubstituteRange::CurrentLine,
                &RegexPattern::compile("(x)").unwrap(),
                "&-\\1-$1",
                SubstituteOptions::default(),
            )
            .unwrap();

        assert_eq!(editor.document().as_bytes(), b"&-\\1-x=1");
    }

    #[test]
    fn skips_an_empty_global_match_after_a_nonempty_match() {
        let mut editor = editor_with(b"abc\ndef");
        let result = editor
            .substitute(
                SubstituteRange::Buffer,
                &RegexPattern::compile(".*").unwrap(),
                "x",
                SubstituteOptions {
                    global: true,
                    count_only: false,
                },
            )
            .unwrap();

        assert_eq!(result.matches, 2);
        assert_eq!(editor.document().as_bytes(), b"x\nx");
    }

    #[test]
    fn linewise_put_preserves_an_empty_line() {
        let mut editor = editor_with(b"");

        editor.put_line_below(b"").unwrap();

        let expected = if cfg!(windows) {
            b"\r\n".as_slice()
        } else {
            b"\n".as_slice()
        };
        assert_eq!(editor.document().as_bytes(), expected);
        assert_eq!(editor.position(), (1, 0));
    }

    #[test]
    fn paste_normalizes_line_endings_to_the_document() {
        let mut editor = editor_with(b"one\r\n");
        editor.move_to_last_line();

        editor.insert_paste("a\nb\r\nc\rd").unwrap();

        assert_eq!(editor.document().as_bytes(), b"one\r\na\r\nb\r\nc\r\nd");
    }

    #[test]
    fn deleting_the_last_line_removes_its_leading_newline() {
        let mut editor = editor_with(b"one\ntwo");
        assert!(editor.move_down(false));

        assert!(editor.delete_line().is_some());

        assert_eq!(editor.document().as_bytes(), b"one");
        assert_eq!(editor.position(), (0, 0));
    }

    #[test]
    fn moves_and_deletes_whole_emoji_graphemes() {
        let mut editor = editor_with("👩🏽‍💻x".as_bytes());

        assert!(editor.move_right(false));
        assert_eq!(editor.position(), (0, 1));
        assert!(editor.move_left());
        assert!(editor.delete_char().is_some());

        assert_eq!(editor.document().as_bytes(), b"x");
        assert_eq!(editor.position(), (0, 0));
    }

    #[test]
    fn moves_to_one_based_lines_and_clamps() {
        let mut editor = editor_with(b"one\ntwo\nthree");

        editor.move_to_line(2);
        assert_eq!(editor.position(), (1, 0));
        editor.move_to_line(99);
        assert_eq!(editor.position(), (2, 0));
        editor.move_to_line(0);
        assert_eq!(editor.position(), (0, 0));
    }

    #[test]
    fn character_selection_is_inclusive_and_grapheme_safe_in_both_directions() {
        let mut editor = editor_with("a👩🏽‍💻b".as_bytes());
        editor.begin_selection();
        assert!(editor.move_right(false));
        assert_eq!(
            editor.selected_bytes(SelectionKind::Character).unwrap(),
            "a👩🏽‍💻".as_bytes()
        );

        editor.clear_selection();
        assert!(editor.move_right(false));
        editor.begin_selection();
        assert!(editor.move_left());
        assert_eq!(
            editor.selected_bytes(SelectionKind::Character).unwrap(),
            "👩🏽‍💻b".as_bytes()
        );
    }

    #[test]
    fn line_selection_handles_crlf_and_the_last_line() {
        let mut editor = editor_with(b"one\r\ntwo\r\nthree");
        assert!(editor.move_down(false));
        editor.begin_selection();
        assert!(editor.move_down(false));

        assert_eq!(
            editor.selected_bytes(SelectionKind::Line).unwrap(),
            b"two\r\nthree"
        );
        editor.checkpoint();
        assert_eq!(
            editor.delete_selection(SelectionKind::Line).unwrap(),
            b"\r\ntwo\r\nthree"
        );
        assert_eq!(editor.document().as_bytes(), b"one");
        assert!(editor.undo());
        assert_eq!(editor.document().as_bytes(), b"one\r\ntwo\r\nthree");
    }

    #[test]
    fn selected_line_substitution_preserves_current_replacement_semantics() {
        let mut editor = editor_with(b"x=1\nx=2\nx=3");
        editor.begin_selection();
        assert!(editor.move_down(false));
        let pattern = RegexPattern::compile("(x)").unwrap();

        let result = editor
            .substitute(
                SubstituteRange::SelectedLines,
                &pattern,
                "$1$1",
                SubstituteOptions::default(),
            )
            .unwrap();

        assert_eq!(result.lines, 2);
        assert_eq!(editor.document().as_bytes(), b"xx=1\nxx=2\nx=3");
    }

    #[test]
    fn supports_redo_and_clears_it_on_a_new_change() {
        let mut editor = editor_with(b"a");
        editor.checkpoint();
        editor.insert(b'b').unwrap();

        assert!(editor.undo());
        assert_eq!(editor.document().as_bytes(), b"a");
        assert!(editor.redo());
        assert_eq!(editor.document().as_bytes(), b"ba");

        assert!(editor.undo());
        editor.checkpoint();
        editor.insert(b'c').unwrap();
        assert!(!editor.redo());
        assert_eq!(editor.document().as_bytes(), b"ca");
    }
}
