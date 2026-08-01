//! View-local navigation state.

use crate::{BufferId, Cursor};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ViewId(pub(crate) u64);

#[derive(Clone, Debug)]
pub struct EditorView {
    pub(crate) buffer_id: BufferId,
    pub(crate) cursor: Cursor,
    pub(crate) preferred_column: Option<usize>,
}

impl EditorView {
    pub fn new(buffer_id: BufferId) -> Self {
        Self {
            buffer_id,
            cursor: Cursor::default(),
            preferred_column: None,
        }
    }

    pub fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub(crate) fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cursor
    }

    pub fn preferred_column(&self) -> Option<usize> {
        self.preferred_column
    }

    pub(crate) fn set_preferred_column(&mut self, column: Option<usize>) {
        self.preferred_column = column;
    }
}
