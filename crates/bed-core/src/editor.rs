//! Editing operations and history.
//!
//! Cursor movement uses extended grapheme clusters when bytes form valid
//! UTF-8, while malformed input remains editable through a byte-safe fallback.
//! Each buffer owns its bounded undo and redo history, while cursor navigation
//! belongs to the active view.

use crate::{Buffer, BufferId, BufferStore, Cursor, Document, EditorView, ViewId};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use unicode_segmentation::UnicodeSegmentation;

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
            return true;
        }
        let Some(next_view) = self.parked_views.remove(&view_id) else {
            return false;
        };
        let previous_id = self.view_id;
        let previous_view = std::mem::replace(&mut self.view, next_view);
        self.view_id = view_id;
        self.parked_views.insert(previous_id, previous_view);
        true
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

    pub fn search_forward(&mut self, query: &str) -> bool {
        let Some(offset) = find_forward_offset(
            self.document().as_bytes(),
            query.as_bytes(),
            self.view.cursor.offset(),
        ) else {
            return false;
        };
        self.view.cursor.set_offset(offset);
        self.normalize_normal_cursor();
        true
    }

    pub fn search_backward(&mut self, query: &str) -> bool {
        let Some(offset) = find_backward_offset(
            self.document().as_bytes(),
            query.as_bytes(),
            self.view.cursor.offset(),
        ) else {
            return false;
        };
        self.view.cursor.set_offset(offset);
        self.normalize_normal_cursor();
        true
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

    pub fn save_all(&mut self) -> Result<usize> {
        let mut written = 0;
        for &buffer_id in &self.buffer_order {
            let buffer = self
                .buffers
                .get_mut(buffer_id)
                .expect("buffer order references a missing buffer");
            if buffer.document().is_dirty() {
                buffer.document_mut().save()?;
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
                .is_some_and(|buffer| buffer.document().is_dirty())
        })
    }

    pub fn dirty_buffer_count(&self) -> usize {
        self.buffer_order
            .iter()
            .filter(|&&buffer_id| {
                self.buffers
                    .get(buffer_id)
                    .is_some_and(|buffer| buffer.document().is_dirty())
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
        let row = self.document().as_bytes()[..offset.min(self.document().len())]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count();
        let start = self.document().line_start(offset);
        let column = grapheme_count(&self.document().as_bytes()[start..offset]);
        (row, column)
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
        self.buffer_order.iter().copied().find(|&buffer_id| {
            self.buffers
                .get(buffer_id)
                .is_some_and(|buffer| buffer.document().path() == path)
        })
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

fn editing_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        offsets.push(offset);
        offset = next_grapheme_offset(bytes, offset);
    }
    offsets
}

fn find_forward_offset(bytes: &[u8], query: &[u8], cursor: usize) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    let offsets = editing_offsets(bytes);
    offsets
        .iter()
        .copied()
        .filter(|offset| *offset > cursor)
        .chain(offsets.iter().copied().filter(|offset| *offset <= cursor))
        .find(|offset| bytes[*offset..].starts_with(query))
}

fn find_backward_offset(bytes: &[u8], query: &[u8], cursor: usize) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    let offsets = editing_offsets(bytes);
    offsets
        .iter()
        .rev()
        .copied()
        .filter(|offset| *offset < cursor)
        .chain(
            offsets
                .iter()
                .rev()
                .copied()
                .filter(|offset| *offset >= cursor),
        )
        .find(|offset| bytes[*offset..].starts_with(query))
}

fn grapheme_count(bytes: &[u8]) -> usize {
    String::from_utf8_lossy(bytes).graphemes(true).count()
}

#[cfg(test)]
mod tests {
    use super::Editor;
    use crate::Document;
    use std::path::PathBuf;

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

        assert!(editor.search_forward("one"));
        assert_eq!(editor.current_line_prefix(), "one 你好 ".as_bytes());
        assert!(editor.search_forward("one"));
        assert_eq!(editor.cursor().offset(), 0);
        assert!(editor.search_backward("你好"));
        assert_eq!(editor.current_line_prefix(), b"one ");
        assert!(!editor.search_forward("missing"));
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
