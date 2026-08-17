//! Incremental filesystem tree state backed by per-directory snapshots.

use crate::filesystem::{ScanKey, ScannedEntry, absolute_path};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub(crate) struct TreeEntry {
    pub(crate) path: PathBuf,
    pub(crate) depth: usize,
    pub(crate) kind: TreeEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeEntryKind {
    Parent,
    Directory,
    File,
}

#[derive(Clone, Debug)]
pub(crate) struct FileTree {
    root: PathBuf,
    entries: Vec<TreeEntry>,
    expanded: HashSet<PathBuf>,
    directories: HashMap<PathBuf, Vec<ScannedEntry>>,
    stale_directories: HashSet<PathBuf>,
    directory_revisions: HashMap<PathBuf, u64>,
    tree_revision: u64,
    selected: usize,
    selected_path: Option<PathBuf>,
    row_offset: usize,
}

impl FileTree {
    pub(crate) fn new(root: PathBuf) -> Self {
        let root = if root.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            root
        };
        let root = absolute_path(&root).unwrap_or(root);
        let mut tree = Self {
            root,
            entries: Vec::new(),
            expanded: HashSet::new(),
            directories: HashMap::new(),
            stale_directories: HashSet::new(),
            directory_revisions: HashMap::new(),
            tree_revision: 0,
            selected: 0,
            selected_path: None,
            row_offset: 0,
        };
        tree.rebuild_entries();
        tree
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn root_label(&self) -> String {
        directory_label(&self.root)
    }

    pub(crate) fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn row_offset(&self) -> usize {
        self.row_offset
    }

    pub(crate) fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    pub(crate) fn set_root(&mut self, root: PathBuf) -> io::Result<()> {
        let root = absolute_path(&root)?;
        if !fs::metadata(&root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        self.root = root;
        self.expanded.clear();
        self.reset();
        self.selected = 0;
        self.selected_path = None;
        self.row_offset = 0;
        Ok(())
    }

    pub(crate) fn set_parent_as_root(&mut self) -> io::Result<()> {
        let Some(parent) = self.root.parent().map(PathBuf::from) else {
            return Ok(());
        };
        self.move_to_parent(parent)
    }

    pub(crate) fn set_selected_as_root(&mut self) -> io::Result<()> {
        let Some(path) = self.selected_path.clone().or_else(|| {
            self.entries
                .get(self.selected)
                .map(|entry| entry.path.clone())
        }) else {
            return Ok(());
        };
        if !fs::metadata(&path)?.is_dir() {
            return Ok(());
        }
        self.set_root(path)
    }

    pub(crate) fn refresh(&mut self) -> io::Result<()> {
        for directory in self.visible_directories() {
            self.invalidate_directory(&directory);
        }
        Ok(())
    }

    pub(crate) fn invalidate_paths<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) {
        let mut directories = HashSet::new();
        for path in paths {
            let Ok(path) = absolute_path(path) else {
                continue;
            };
            if path == self.root || self.expanded.contains(&path) {
                directories.insert(path.clone());
            }
            if let Some(parent) = path.parent()
                && (parent == self.root || self.expanded.contains(parent))
            {
                directories.insert(parent.to_path_buf());
            }
        }
        for directory in directories {
            self.invalidate_directory(&directory);
        }
    }

    pub(crate) fn watched_directories(&self) -> HashSet<PathBuf> {
        self.visible_directories().into_iter().collect()
    }

    pub(crate) fn scan_requests(&self, tab: u64) -> Vec<ScanKey> {
        self.visible_directories()
            .into_iter()
            .filter(|path| {
                !self.directories.contains_key(path) || self.stale_directories.contains(path)
            })
            .map(|path| ScanKey {
                tab,
                tree_revision: self.tree_revision,
                directory_revision: self.directory_revision(&path),
                path,
            })
            .collect()
    }

    pub(crate) fn apply_scan(
        &mut self,
        key: &ScanKey,
        entries: io::Result<Vec<ScannedEntry>>,
    ) -> Result<bool, String> {
        if key.tree_revision != self.tree_revision
            || key.directory_revision != self.directory_revision(&key.path)
        {
            return Ok(false);
        }
        match entries {
            Ok(entries) => {
                self.stale_directories.remove(&key.path);
                let changed = self.directories.get(&key.path) != Some(&entries);
                self.directories.insert(key.path.clone(), entries);
                if changed {
                    self.rebuild_entries();
                }
                Ok(changed)
            }
            Err(error) => {
                self.stale_directories.remove(&key.path);
                if error.kind() == io::ErrorKind::NotFound {
                    let changed = self
                        .directories
                        .insert(key.path.clone(), Vec::new())
                        .is_some_and(|entries| !entries.is_empty());
                    if changed {
                        self.rebuild_entries();
                    }
                } else {
                    self.directories.entry(key.path.clone()).or_default();
                }
                Err(format!("failed to read {}: {error}", key.path.display()))
            }
        }
    }

    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.remember_selected_path();
    }

    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
        self.remember_selected_path();
    }

    pub(crate) fn activate(&mut self) -> io::Result<Option<PathBuf>> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(None);
        };
        match entry.kind {
            TreeEntryKind::Parent => {
                self.move_to_parent(entry.path)?;
                return Ok(None);
            }
            TreeEntryKind::File => return Ok(Some(entry.path)),
            TreeEntryKind::Directory => {}
        }

        if self.expanded.remove(&entry.path) {
            self.rebuild_entries();
        } else {
            self.expanded.insert(entry.path);
            self.rebuild_entries();
        }
        Ok(None)
    }

    pub(crate) fn collapse(&mut self) -> io::Result<()> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(());
        };
        if entry.kind == TreeEntryKind::Parent {
            return self.move_to_parent(entry.path);
        }
        if entry.kind == TreeEntryKind::Directory && self.expanded.remove(&entry.path) {
            self.rebuild_entries();
            return Ok(());
        }
        if entry.depth == 0 {
            return Ok(());
        }
        if let Some(parent) = self.entries[..self.selected]
            .iter()
            .rposition(|candidate| candidate.depth < entry.depth)
        {
            self.selected = parent;
            self.remember_selected_path();
        }
        Ok(())
    }

    pub(crate) fn ensure_visible(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        if self.selected < self.row_offset {
            self.row_offset = self.selected;
        } else if self.selected >= self.row_offset + rows {
            self.row_offset = self.selected - rows + 1;
        }
    }

    fn move_to_parent(&mut self, parent: PathBuf) -> io::Result<()> {
        let previous_root = self.root.clone();
        self.set_root(parent)?;
        self.selected_path = Some(previous_root);
        Ok(())
    }

    fn reset(&mut self) {
        self.tree_revision = self.tree_revision.wrapping_add(1);
        self.directories.clear();
        self.stale_directories.clear();
        self.directory_revisions.clear();
        self.rebuild_entries();
    }

    fn invalidate_directory(&mut self, directory: &Path) {
        let revision = self
            .directory_revisions
            .entry(directory.to_path_buf())
            .or_default();
        *revision = revision.wrapping_add(1);
        self.stale_directories.insert(directory.to_path_buf());
    }

    fn directory_revision(&self, path: &Path) -> u64 {
        self.directory_revisions.get(path).copied().unwrap_or(0)
    }

    fn visible_directories(&self) -> Vec<PathBuf> {
        let mut directories = vec![self.root.clone()];
        self.collect_visible_directories(&self.root, &mut directories);
        directories
    }

    fn collect_visible_directories(&self, directory: &Path, output: &mut Vec<PathBuf>) {
        let Some(entries) = self.directories.get(directory) else {
            return;
        };
        for entry in entries {
            if entry.is_directory && self.expanded.contains(&entry.path) {
                output.push(entry.path.clone());
                self.collect_visible_directories(&entry.path, output);
            }
        }
    }

    fn rebuild_entries(&mut self) {
        let selected_path = self.selected_path.clone().or_else(|| {
            self.entries
                .get(self.selected)
                .map(|entry| entry.path.clone())
        });
        let mut entries = Vec::new();
        if let Some(parent) = self.root.parent().map(PathBuf::from) {
            entries.push(TreeEntry {
                path: parent,
                depth: 0,
                kind: TreeEntryKind::Parent,
            });
        }
        self.collect_entries(&self.root, 0, &mut entries);
        self.entries = entries;
        if let Some(path) = selected_path
            && let Some(index) = self.entries.iter().position(|entry| entry.path == path)
        {
            self.selected = index;
            self.selected_path = Some(path);
            return;
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    fn collect_entries(&self, directory: &Path, depth: usize, output: &mut Vec<TreeEntry>) {
        let Some(entries) = self.directories.get(directory) else {
            return;
        };
        for entry in entries {
            output.push(TreeEntry {
                path: entry.path.clone(),
                depth,
                kind: if entry.is_directory {
                    TreeEntryKind::Directory
                } else {
                    TreeEntryKind::File
                },
            });
            if entry.is_directory && self.expanded.contains(&entry.path) {
                self.collect_entries(&entry.path, depth + 1, output);
            }
        }
    }

    fn remember_selected_path(&mut self) {
        self.selected_path = self
            .entries
            .get(self.selected)
            .map(|entry| entry.path.clone());
    }
}

fn directory_label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{FileTree, TreeEntryKind, directory_label};
    use crate::filesystem::{ScanKey, ScannedEntry};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    fn load_visible(tree: &mut FileTree) {
        loop {
            let requests = tree.scan_requests(0);
            if requests.is_empty() {
                break;
            }
            for key in requests {
                let entries = scan(&key.path);
                tree.apply_scan(&key, Ok(entries)).unwrap();
            }
        }
    }

    fn scan(path: &Path) -> Vec<ScannedEntry> {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                ScannedEntry {
                    path: entry.path(),
                    is_directory: entry.file_type().unwrap().is_dir(),
                }
            })
            .collect();
        entries.sort_by_cached_key(|entry| {
            (
                !entry.is_directory,
                entry.path.file_name().unwrap().to_os_string(),
            )
        });
        entries
    }

    #[test]
    fn sorts_expands_and_collapses_directories() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("bed-tree-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("directory")).unwrap();
        fs::write(root.join("directory").join("nested"), b"").unwrap();
        fs::write(root.join("file"), b"").unwrap();

        let mut tree = FileTree::new(root.clone());
        load_visible(&mut tree);
        assert_eq!(tree.entries().len(), 3);
        assert_eq!(tree.entries()[0].kind, TreeEntryKind::Parent);
        assert_eq!(tree.entries()[1].kind, TreeEntryKind::Directory);
        tree.move_down();
        tree.activate().unwrap();
        load_visible(&mut tree);
        assert_eq!(tree.entries().len(), 4);
        assert_eq!(tree.entries()[2].depth, 1);
        tree.move_down();
        tree.collapse().unwrap();
        assert_eq!(tree.selected(), 1);
        tree.collapse().unwrap();
        assert_eq!(tree.entries().len(), 3);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parent_entry_changes_root_and_selects_the_previous_directory() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent =
            std::env::temp_dir().join(format!("bed-tree-parent-{}-{nonce}", std::process::id()));
        let root = parent.join("child");
        fs::create_dir_all(&root).unwrap();

        let mut tree = FileTree::new(root.clone());
        load_visible(&mut tree);
        tree.activate().unwrap();

        tree.set_selected_as_root().unwrap();
        assert_eq!(tree.root(), root);
        tree.set_parent_as_root().unwrap();
        load_visible(&mut tree);

        assert_eq!(tree.root(), parent);
        assert_eq!(tree.entries()[tree.selected()].path, root);
        assert_eq!(
            tree.entries()[tree.selected()].kind,
            TreeEntryKind::Directory
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn ignores_stale_scan_results_after_invalidation() {
        let root = std::env::temp_dir();
        let mut tree = FileTree::new(root.clone());
        let stale = tree.scan_requests(3).pop().unwrap();
        tree.invalidate_paths(std::iter::once(root.as_path()));

        assert!(!tree.apply_scan(&stale, Ok(Vec::new())).unwrap());
        let current = tree.scan_requests(3).pop().unwrap();
        assert_ne!(stale.directory_revision, current.directory_revision);
    }

    #[test]
    fn keeps_the_previous_snapshot_until_a_changed_scan_arrives() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("bed-tree-stable-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let original = root.join("original");
        fs::write(&original, b"").unwrap();
        let mut tree = FileTree::new(root.clone());
        load_visible(&mut tree);
        let previous: Vec<_> = tree
            .entries()
            .iter()
            .map(|entry| entry.path.clone())
            .collect();

        tree.invalidate_paths(std::iter::once(original.as_path()));
        assert_eq!(
            tree.entries()
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            previous
        );
        let unchanged = tree.scan_requests(0).pop().unwrap();
        assert!(!tree.apply_scan(&unchanged, Ok(scan(&root))).unwrap());

        let created = root.join("created");
        fs::write(&created, b"").unwrap();
        tree.invalidate_paths(std::iter::once(created.as_path()));
        let changed = tree.scan_requests(0).pop().unwrap();
        assert!(tree.apply_scan(&changed, Ok(scan(&root))).unwrap());
        assert!(tree.entries().iter().any(|entry| entry.path == created));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn labels_relative_directories_and_filesystem_roots() {
        let current = std::env::current_dir().unwrap();
        assert_eq!(directory_label(PathBuf::from(".").as_path()), ".");

        let mut root = current;
        while let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
        assert_eq!(directory_label(&root), root.to_string_lossy());
    }

    #[allow(dead_code)]
    fn assert_scan_key_is_send(_: ScanKey) {}
}
