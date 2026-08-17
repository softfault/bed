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
mod pattern;
mod view;

pub use buffer::{Buffer, BufferId, BufferStore};
pub use cursor::Cursor;
pub use document::{DiskReconcile, Document, ExternalState};
pub use editor::{Editor, LineShift, SubstituteOptions, SubstituteRange, SubstituteResult};
pub use pattern::RegexPattern;
pub use view::{EditorView, SelectionKind, ViewId};
