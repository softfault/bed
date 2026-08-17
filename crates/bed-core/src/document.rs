//! Byte-preserving document storage.
//!
//! Files are not required to be valid UTF-8. Text interpretation belongs to
//! `Editor` and the renderer, while this layer provides checked byte edits,
//! line boundaries, dirty tracking, and persistence.

use anyhow::{Context, Result, ensure};
use std::{
    cell::OnceCell,
    fs,
    io::ErrorKind,
    ops::Range,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExternalState {
    #[default]
    InSync,
    Modified,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskReconcile {
    Unchanged,
    Reloaded,
    Reconciled,
    Conflict,
    Deleted,
}

#[derive(Debug)]
pub struct Document {
    path: PathBuf,
    file_backed: bool,
    bytes: Vec<u8>,
    saved_bytes: Vec<u8>,
    saved_file_state: SavedFileState,
    external_state: ExternalState,
    line_ending: LineEnding,
    line_starts: OnceCell<Vec<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavedFileState {
    Untracked,
    Missing,
    Present,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
}

impl Document {
    pub fn new(path: PathBuf, bytes: Vec<u8>) -> Self {
        let saved_bytes = bytes.clone();
        let line_ending = detect_line_ending(&bytes);
        Self {
            path,
            file_backed: true,
            bytes,
            saved_bytes,
            saved_file_state: SavedFileState::Untracked,
            external_state: ExternalState::InSync,
            line_ending,
            line_starts: OnceCell::new(),
        }
    }

    pub fn scratch() -> Self {
        let mut document = Self::new(PathBuf::from("[No Name]"), Vec::new());
        document.file_backed = false;
        document
    }

    pub fn open(path: PathBuf) -> Result<Self> {
        let (bytes, saved_file_state) = match fs::read(&path) {
            Ok(bytes) => (bytes, SavedFileState::Present),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                (Vec::new(), SavedFileState::Missing)
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        let mut document = Self::new(path, bytes);
        document.saved_file_state = saved_file_state;
        Ok(document)
    }

    pub fn insert(&mut self, offset: usize, byte: u8) -> Result<()> {
        ensure!(
            offset <= self.bytes.len(),
            "cannot insert at byte offset {offset}: document length is {}",
            self.bytes.len()
        );

        self.bytes.insert(offset, byte);
        self.invalidate_line_starts();
        Ok(())
    }

    pub fn insert_bytes(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        ensure!(
            offset <= self.bytes.len(),
            "cannot insert at byte offset {offset}: document length is {}",
            self.bytes.len()
        );

        if !bytes.is_empty() {
            self.bytes.splice(offset..offset, bytes.iter().copied());
            self.invalidate_line_starts();
        }
        Ok(())
    }

    pub fn delete(&mut self, offset: usize) -> Option<u8> {
        if offset >= self.bytes.len() {
            return None;
        }

        let byte = self.bytes.remove(offset);
        self.invalidate_line_starts();
        Some(byte)
    }

    pub fn delete_range(&mut self, range: Range<usize>) -> Option<Vec<u8>> {
        if range.start >= range.end || range.end > self.bytes.len() {
            return None;
        }

        let bytes = self.bytes.drain(range).collect();
        self.invalidate_line_starts();
        Some(bytes)
    }

    pub fn save(&mut self) -> Result<()> {
        self.save_impl(false)
    }

    pub fn save_force(&mut self) -> Result<()> {
        self.save_impl(true)
    }

    fn save_impl(&mut self, force: bool) -> Result<()> {
        ensure!(self.file_backed, "scratch buffer has no file name");
        if force || self.saved_file_state == SavedFileState::Untracked {
            bed_file::atomic_write(&self.path, &self.bytes)
        } else {
            let expected = match self.saved_file_state {
                SavedFileState::Missing => None,
                SavedFileState::Present => Some(self.saved_bytes.as_slice()),
                SavedFileState::Untracked => unreachable!("handled above"),
            };
            bed_file::atomic_write_if_unchanged(&self.path, expected, &self.bytes)
        }
        .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.saved_bytes.clone_from(&self.bytes);
        self.saved_file_state = SavedFileState::Present;
        self.external_state = ExternalState::InSync;
        Ok(())
    }

    pub fn reconcile_disk(&mut self) -> Result<DiskReconcile> {
        if !self.file_backed {
            return Ok(DiskReconcile::Unchanged);
        }

        let disk_bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if self.saved_file_state != SavedFileState::Present
                    && self.external_state == ExternalState::InSync
                {
                    return Ok(DiskReconcile::Unchanged);
                }
                let changed = self.external_state != ExternalState::Deleted;
                self.external_state = ExternalState::Deleted;
                return Ok(if changed {
                    DiskReconcile::Deleted
                } else {
                    DiskReconcile::Unchanged
                });
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", self.path.display()));
            }
        };

        if disk_bytes == self.bytes {
            let changed = self.saved_bytes != disk_bytes
                || self.saved_file_state != SavedFileState::Present
                || self.external_state != ExternalState::InSync;
            self.saved_bytes = disk_bytes;
            self.saved_file_state = SavedFileState::Present;
            self.external_state = ExternalState::InSync;
            return Ok(if changed {
                DiskReconcile::Reconciled
            } else {
                DiskReconcile::Unchanged
            });
        }

        if !self.is_dirty() && self.external_state != ExternalState::Modified {
            self.bytes = disk_bytes.clone();
            self.saved_bytes = disk_bytes;
            self.saved_file_state = SavedFileState::Present;
            self.external_state = ExternalState::InSync;
            self.line_ending = detect_line_ending(&self.bytes);
            self.invalidate_line_starts();
            return Ok(DiskReconcile::Reloaded);
        }

        if disk_bytes == self.saved_bytes && self.external_state != ExternalState::Modified {
            let changed = self.external_state != ExternalState::InSync
                || self.saved_file_state != SavedFileState::Present;
            self.saved_file_state = SavedFileState::Present;
            self.external_state = ExternalState::InSync;
            return Ok(if changed {
                DiskReconcile::Reconciled
            } else {
                DiskReconcile::Unchanged
            });
        }

        let changed = self.external_state != ExternalState::Modified;
        self.saved_file_state = SavedFileState::Present;
        self.external_state = ExternalState::Modified;
        Ok(if changed {
            DiskReconcile::Conflict
        } else {
            DiskReconcile::Unchanged
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_file_backed(&self) -> bool {
        self.file_backed
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.bytes != self.saved_bytes
    }

    pub fn external_state(&self) -> ExternalState {
        self.external_state
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.is_dirty() || self.external_state != ExternalState::InSync
    }

    pub fn line_count(&self) -> usize {
        self.line_starts().len()
    }

    pub fn line(&self, row: usize) -> Option<&[u8]> {
        let start = self.line_start_by_row(row)?;
        let end = self.line_end(start);
        Some(&self.bytes[start..end])
    }

    pub(crate) fn line_start(&self, offset: usize) -> usize {
        self.line_starts()[self.row_for_offset(offset)]
    }

    pub(crate) fn line_end(&self, offset: usize) -> usize {
        let row = self.row_for_offset(offset);
        let end = self
            .line_starts()
            .get(row + 1)
            .map_or(self.bytes.len(), |next_start| next_start - 1);
        if end > 0 && self.bytes.get(end - 1) == Some(&b'\r') {
            end - 1
        } else {
            end
        }
    }

    pub(crate) fn line_break_end(&self, line_end: usize) -> usize {
        match self.bytes.get(line_end..) {
            Some([b'\r', b'\n', ..]) => line_end + 2,
            Some([b'\n', ..]) => line_end + 1,
            _ => line_end,
        }
    }

    pub(crate) fn preceding_line_break_start(&self, line_start: usize) -> usize {
        if line_start >= 2 && self.bytes.get(line_start - 2..line_start) == Some(b"\r\n") {
            line_start - 2
        } else {
            line_start.saturating_sub(1)
        }
    }

    pub(crate) fn line_ending(&self) -> &'static [u8] {
        match self.line_ending {
            LineEnding::Lf => b"\n",
            LineEnding::CrLf => b"\r\n",
        }
    }

    pub(crate) fn line_start_by_row(&self, row: usize) -> Option<usize> {
        self.line_starts().get(row).copied()
    }

    pub(crate) fn row_for_offset(&self, offset: usize) -> usize {
        self.line_starts()
            .partition_point(|&start| start <= offset.min(self.bytes.len()))
            .saturating_sub(1)
    }

    pub(crate) fn restore(&mut self, bytes: Vec<u8>) {
        self.bytes = bytes;
        self.invalidate_line_starts();
    }

    fn line_starts(&self) -> &[usize] {
        self.line_starts.get_or_init(|| {
            let mut starts =
                Vec::with_capacity(self.bytes.iter().filter(|&&byte| byte == b'\n').count() + 1);
            starts.push(0);
            starts.extend(
                self.bytes
                    .iter()
                    .enumerate()
                    .filter_map(|(offset, &byte)| (byte == b'\n').then_some(offset + 1)),
            );
            starts
        })
    }

    fn invalidate_line_starts(&mut self) {
        self.line_starts.take();
    }
}

fn detect_line_ending(bytes: &[u8]) -> LineEnding {
    for (offset, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            return if offset > 0 && bytes[offset - 1] == b'\r' {
                LineEnding::CrLf
            } else {
                LineEnding::Lf
            };
        }
    }

    if cfg!(windows) {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    }
}

#[cfg(test)]
mod tests {
    use super::{DiskReconcile, Document, ExternalState};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new() -> Self {
            let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("bed-{}-{id}.txt", std::process::id()));
            Self(path)
        }

        fn path(&self) -> PathBuf {
            self.0.clone()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn inserts_and_deletes_bytes() {
        let mut document = Document::new(PathBuf::from("test.txt"), b"abc".to_vec());

        document.insert(1, b'X').unwrap();
        assert_eq!(document.as_bytes(), b"aXbc");
        assert!(document.is_dirty());

        assert_eq!(document.delete(2), Some(b'b'));
        assert_eq!(document.as_bytes(), b"aXc");
    }

    #[test]
    fn rejects_edits_beyond_the_document() {
        let mut document = Document::new(PathBuf::from("test.txt"), b"abc".to_vec());

        assert!(document.insert(4, b'X').is_err());
        assert_eq!(document.delete(3), None);
        assert_eq!(document.as_bytes(), b"abc");
        assert!(!document.is_dirty());
    }

    #[test]
    fn opens_edits_and_saves_a_file() {
        let temp_file = TempFile::new();
        fs::write(&temp_file.0, b"abc").unwrap();

        let mut document = Document::open(temp_file.path()).unwrap();
        assert!(!document.is_dirty());

        document.insert(document.len(), b'd').unwrap();
        document.save().unwrap();

        assert_eq!(fs::read(&temp_file.0).unwrap(), b"abcd");
        assert!(!document.is_dirty());
    }

    #[test]
    fn refuses_to_overwrite_a_file_modified_after_opening() {
        let temp_file = TempFile::new();
        fs::write(&temp_file.0, b"original").unwrap();
        let mut document = Document::open(temp_file.path()).unwrap();
        document.insert_bytes(document.len(), b" edited").unwrap();
        fs::write(&temp_file.0, b"external").unwrap();

        let error = document.save().unwrap_err();

        assert!(format!("{error:#}").contains("changed on disk"));
        assert_eq!(fs::read(&temp_file.0).unwrap(), b"external");
        assert!(document.is_dirty());

        document.save_force().unwrap();
        assert_eq!(fs::read(&temp_file.0).unwrap(), b"original edited");
        assert!(!document.is_dirty());
    }

    #[test]
    fn detects_deletion_and_creation_conflicts() {
        let deleted_file = TempFile::new();
        fs::write(&deleted_file.0, b"original").unwrap();
        let mut deleted = Document::open(deleted_file.path()).unwrap();
        deleted.insert(0, b'X').unwrap();
        fs::remove_file(&deleted_file.0).unwrap();
        assert!(format!("{:#}", deleted.save().unwrap_err()).contains("changed on disk"));

        let created_file = TempFile::new();
        let mut created = Document::open(created_file.path()).unwrap();
        created.insert_bytes(0, b"mine").unwrap();
        fs::write(&created_file.0, b"theirs").unwrap();
        assert!(format!("{:#}", created.save().unwrap_err()).contains("changed on disk"));
        assert_eq!(fs::read(&created_file.0).unwrap(), b"theirs");
    }

    #[test]
    fn permits_a_disk_rewrite_when_the_content_is_unchanged() {
        let temp_file = TempFile::new();
        fs::write(&temp_file.0, b"original").unwrap();
        let mut document = Document::open(temp_file.path()).unwrap();
        document.insert_bytes(document.len(), b" edited").unwrap();
        fs::write(&temp_file.0, b"original").unwrap();

        document.save().unwrap();

        assert_eq!(fs::read(&temp_file.0).unwrap(), b"original edited");
    }

    #[test]
    fn opens_a_missing_path_as_an_empty_document() {
        let temp_file = TempFile::new();

        let document = Document::open(temp_file.path()).unwrap();

        assert!(document.is_empty());
        assert!(!document.is_dirty());
    }

    #[test]
    fn reloads_clean_documents_changed_on_disk() {
        let temp_file = TempFile::new();
        fs::write(&temp_file.0, b"original").unwrap();
        let mut document = Document::open(temp_file.path()).unwrap();
        fs::write(&temp_file.0, b"external\r\nchange").unwrap();

        assert_eq!(document.reconcile_disk().unwrap(), DiskReconcile::Reloaded);
        assert_eq!(document.as_bytes(), b"external\r\nchange");
        assert_eq!(document.line_ending(), b"\r\n");
        assert_eq!(document.external_state(), ExternalState::InSync);
        assert!(!document.has_unsaved_changes());
    }

    #[test]
    fn preserves_dirty_documents_and_marks_external_conflicts() {
        let temp_file = TempFile::new();
        fs::write(&temp_file.0, b"original").unwrap();
        let mut document = Document::open(temp_file.path()).unwrap();
        document.insert_bytes(document.len(), b" local").unwrap();
        fs::write(&temp_file.0, b"external").unwrap();

        assert_eq!(document.reconcile_disk().unwrap(), DiskReconcile::Conflict);
        assert_eq!(document.as_bytes(), b"original local");
        assert_eq!(document.external_state(), ExternalState::Modified);
        assert!(document.has_unsaved_changes());
        assert_eq!(document.reconcile_disk().unwrap(), DiskReconcile::Unchanged);

        document.save_force().unwrap();
        assert_eq!(document.external_state(), ExternalState::InSync);
    }

    #[test]
    fn preserves_documents_deleted_on_disk() {
        let temp_file = TempFile::new();
        fs::write(&temp_file.0, b"only copy").unwrap();
        let mut document = Document::open(temp_file.path()).unwrap();
        fs::remove_file(&temp_file.0).unwrap();

        assert_eq!(document.reconcile_disk().unwrap(), DiskReconcile::Deleted);
        assert_eq!(document.as_bytes(), b"only copy");
        assert_eq!(document.external_state(), ExternalState::Deleted);
        assert!(document.has_unsaved_changes());
    }

    #[test]
    fn silently_reconciles_when_disk_matches_local_content() {
        let temp_file = TempFile::new();
        fs::write(&temp_file.0, b"original").unwrap();
        let mut document = Document::open(temp_file.path()).unwrap();
        document.insert_bytes(document.len(), b" local").unwrap();
        fs::write(&temp_file.0, b"original local").unwrap();

        assert_eq!(
            document.reconcile_disk().unwrap(),
            DiskReconcile::Reconciled
        );
        assert!(!document.has_unsaved_changes());
        assert_eq!(document.external_state(), ExternalState::InSync);
    }

    #[test]
    fn scratch_documents_cannot_be_written_to_a_display_label() {
        let mut document = Document::scratch();
        document.insert_bytes(0, b"draft").unwrap();

        assert!(!document.is_file_backed());
        assert!(format!("{:#}", document.save_force().unwrap_err()).contains("no file name"));
        assert!(document.is_dirty());
    }

    #[test]
    fn restored_saved_content_is_not_dirty() {
        let mut document = Document::new(PathBuf::from("test.txt"), b"abc".to_vec());
        document.insert(document.len(), b'd').unwrap();
        assert!(document.is_dirty());

        document.restore(b"abc".to_vec());

        assert!(!document.is_dirty());
    }

    #[test]
    fn exposes_text_lines() {
        let mut document = Document::new(PathBuf::from("test.txt"), b"one\ntwo\n".to_vec());

        assert_eq!(document.line_count(), 3);
        assert_eq!(document.line(0), Some(b"one".as_slice()));
        assert_eq!(document.line(1), Some(b"two".as_slice()));
        assert_eq!(document.line(2), Some(b"".as_slice()));
        assert_eq!(document.line(3), None);

        document.insert_bytes(0, b"zero\n").unwrap();
        assert_eq!(document.line_count(), 4);
        assert_eq!(document.line(0), Some(b"zero".as_slice()));
        assert_eq!(document.line(3), Some(b"".as_slice()));

        document.delete_range(0..5).unwrap();
        assert_eq!(document.line_count(), 3);
        assert_eq!(document.line(0), Some(b"one".as_slice()));

        document.restore(b"last".to_vec());
        assert_eq!(document.line_count(), 1);
        assert_eq!(document.line(0), Some(b"last".as_slice()));
    }

    #[test]
    fn excludes_crlf_separators_from_lines() {
        let document = Document::new(PathBuf::from("test.txt"), b"one\r\ntwo\r\n".to_vec());

        assert_eq!(document.line_count(), 3);
        assert_eq!(document.line(0), Some(b"one".as_slice()));
        assert_eq!(document.line(1), Some(b"two".as_slice()));
        assert_eq!(document.line(2), Some(b"".as_slice()));
        assert_eq!(document.line_end(0), 3);
        assert_eq!(document.line_break_end(3), 5);
        assert_eq!(document.preceding_line_break_start(5), 3);
        assert_eq!(document.line_ending(), b"\r\n");
    }
}
