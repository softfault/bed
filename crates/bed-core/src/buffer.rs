//! Buffer storage and buffer-local edit history.

use crate::Document;
use std::collections::HashMap;

// Snapshot history is deliberately bounded because every entry owns a full
// document copy.
const HISTORY_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BufferId(u64);

#[derive(Debug)]
struct Snapshot {
    bytes: Vec<u8>,
    cursor: usize,
}

#[derive(Debug)]
pub struct Buffer {
    document: Document,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl Buffer {
    pub fn new(document: Document) -> Self {
        Self {
            document,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub(crate) fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }

    pub(crate) fn checkpoint(&mut self, cursor: usize) {
        // A new edit branch invalidates redo. Duplicate byte snapshots are
        // skipped so entering and leaving insert mode without edits is a no-op.
        self.redo.clear();
        let snapshot = self.snapshot(cursor);
        if self
            .undo
            .last()
            .is_some_and(|last| last.bytes == snapshot.bytes)
        {
            return;
        }
        push_snapshot(&mut self.undo, snapshot);
    }

    pub(crate) fn undo(&mut self, cursor: usize) -> Option<usize> {
        let snapshot = self.undo.pop()?;
        let current = self.snapshot(cursor);
        push_snapshot(&mut self.redo, current);
        Some(self.restore(snapshot))
    }

    pub(crate) fn redo(&mut self, cursor: usize) -> Option<usize> {
        let snapshot = self.redo.pop()?;
        let current = self.snapshot(cursor);
        push_snapshot(&mut self.undo, current);
        Some(self.restore(snapshot))
    }

    fn snapshot(&self, cursor: usize) -> Snapshot {
        Snapshot {
            bytes: self.document.as_bytes().to_vec(),
            cursor,
        }
    }

    fn restore(&mut self, snapshot: Snapshot) -> usize {
        self.document.restore(snapshot.bytes);
        snapshot.cursor.min(self.document.len())
    }
}

#[derive(Debug, Default)]
pub struct BufferStore {
    buffers: HashMap<BufferId, Buffer>,
    next_id: u64,
}

impl BufferStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, document: Document) -> BufferId {
        let id = BufferId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("buffer ID space exhausted");
        let previous = self.buffers.insert(id, Buffer::new(document));
        debug_assert!(previous.is_none());
        id
    }

    pub fn get(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.get(&id)
    }

    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    pub(crate) fn get_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        self.buffers.get_mut(&id)
    }

    pub(crate) fn remove(&mut self, id: BufferId) -> Option<Buffer> {
        self.buffers.remove(&id)
    }
}

fn push_snapshot(history: &mut Vec<Snapshot>, snapshot: Snapshot) {
    if history.len() == HISTORY_LIMIT {
        history.remove(0);
    }
    history.push(snapshot);
}

#[cfg(test)]
mod tests {
    use super::BufferStore;
    use crate::Document;
    use std::path::PathBuf;

    #[test]
    fn stores_documents_under_distinct_stable_ids() {
        let mut buffers = BufferStore::new();
        let first = buffers.insert(Document::new(PathBuf::from("one"), b"one".to_vec()));
        let second = buffers.insert(Document::new(PathBuf::from("two"), b"two".to_vec()));

        assert_ne!(first, second);
        assert_eq!(buffers.len(), 2);
        assert_eq!(buffers.get(first).unwrap().document().as_bytes(), b"one");
        assert_eq!(buffers.get(second).unwrap().document().as_bytes(), b"two");
    }

    #[test]
    fn histories_are_owned_by_their_buffers() {
        let mut buffers = BufferStore::new();
        let first = buffers.insert(Document::new(PathBuf::from("one"), b"one".to_vec()));
        let second = buffers.insert(Document::new(PathBuf::from("two"), b"two".to_vec()));

        let first_buffer = buffers.get_mut(first).unwrap();
        first_buffer.checkpoint(0);
        first_buffer.document_mut().insert(0, b'X').unwrap();
        assert_eq!(first_buffer.undo(1), Some(0));

        assert_eq!(buffers.get(first).unwrap().document().as_bytes(), b"one");
        assert_eq!(buffers.get(second).unwrap().document().as_bytes(), b"two");
        assert_eq!(buffers.get_mut(second).unwrap().undo(0), None);
    }
}
