//! Byte-offset cursor used by the editing core.
//!
//! `Editor` is responsible for keeping this offset on an editing boundary;
//! `Cursor` only enforces the document-length bound needed by byte operations.

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    offset: usize,
}

impl Cursor {
    pub fn offset(self) -> usize {
        self.offset
    }

    pub(crate) fn move_right(&mut self, document_len: usize) -> bool {
        if self.offset >= document_len {
            return false;
        }

        self.offset += 1;
        true
    }

    pub(crate) fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }
}
