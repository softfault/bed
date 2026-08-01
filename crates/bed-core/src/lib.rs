//! Platform-independent document and editing state.
//!
//! A [`Buffer`] owns a byte-preserving [`Document`] and bounded snapshot
//! history. [`Editor`] combines it with grapheme-aware, view-local navigation
//! without depending on a terminal.

#![forbid(unsafe_code)]

mod buffer;
mod cursor;
mod document;
mod editor;
mod view;

pub use buffer::{Buffer, BufferId, BufferStore};
pub use cursor::Cursor;
pub use document::Document;
pub use editor::Editor;
pub use view::{EditorView, ViewId};
