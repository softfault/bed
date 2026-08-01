//! Dependency-free filesystem tree state.

use std::{
    collections::HashSet,
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
    selected: usize,
    row_offset: usize,
}

impl FileTree {
    pub(crate) fn new(root: PathBuf) -> Self {
        let root = if root.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            root
        };
        let mut tree = Self {
            root,
            entries: Vec::new(),
            expanded: HashSet::new(),
            selected: 0,
            row_offset: 0,
        };
        let _ = tree.refresh();
        tree
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
        let previous = std::mem::replace(&mut self.root, root);
        let previous_expanded = std::mem::take(&mut self.expanded);
        if let Err(error) = self.refresh() {
            self.root = previous;
            self.expanded = previous_expanded;
            return Err(error);
        }
        self.selected = 0;
        self.row_offset = 0;
        Ok(())
    }

    pub(crate) fn refresh(&mut self) -> io::Result<()> {
        let selected_path = self
            .entries
            .get(self.selected)
            .map(|entry| entry.path.clone());
        let mut entries = Vec::new();
        if let Some(parent) = absolute_parent(&self.root)? {
            entries.push(TreeEntry {
                path: parent,
                depth: 0,
                kind: TreeEntryKind::Parent,
            });
        }
        collect_entries(&self.root, 0, &self.expanded, &mut entries)?;
        self.entries = entries;
        self.selected = selected_path
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| self.selected.min(self.entries.len().saturating_sub(1)));
        Ok(())
    }

    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
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

        let was_expanded = self.expanded.contains(&entry.path);
        if was_expanded {
            self.expanded.remove(&entry.path);
        } else {
            self.expanded.insert(entry.path.clone());
        }
        if let Err(error) = self.refresh() {
            if was_expanded {
                self.expanded.insert(entry.path);
            } else {
                self.expanded.remove(&entry.path);
            }
            return Err(error);
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
            return self.refresh();
        }
        if entry.depth == 0 {
            return Ok(());
        }
        if let Some(parent) = self.entries[..self.selected]
            .iter()
            .rposition(|candidate| candidate.depth < entry.depth)
        {
            self.selected = parent;
        }
        Ok(())
    }

    fn move_to_parent(&mut self, parent: PathBuf) -> io::Result<()> {
        let previous_root = std::path::absolute(&self.root)?;
        self.set_root(parent)?;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.path == previous_root)
        {
            self.selected = index;
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
}

fn collect_entries(
    directory: &Path,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    output: &mut Vec<TreeEntry>,
) -> io::Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_cached_key(|entry| {
        let is_directory = entry.file_type().is_ok_and(|file_type| file_type.is_dir());
        (!is_directory, entry.file_name())
    });

    for entry in entries {
        let path = entry.path();
        let is_directory = entry.file_type()?.is_dir();
        output.push(TreeEntry {
            path: path.clone(),
            depth,
            kind: if is_directory {
                TreeEntryKind::Directory
            } else {
                TreeEntryKind::File
            },
        });
        if is_directory && expanded.contains(&path) {
            collect_entries(&path, depth + 1, expanded, output)?;
        }
    }
    Ok(())
}

fn absolute_parent(path: &Path) -> io::Result<Option<PathBuf>> {
    Ok(std::path::absolute(path)?.parent().map(PathBuf::from))
}

fn directory_label(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute
        .file_name()
        .unwrap_or_else(|| absolute.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{FileTree, TreeEntryKind, directory_label};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

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
        assert_eq!(tree.entries().len(), 3);
        assert_eq!(tree.entries()[0].kind, TreeEntryKind::Parent);
        assert_eq!(tree.entries()[1].kind, TreeEntryKind::Directory);
        tree.move_down();
        tree.activate().unwrap();
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
        assert_eq!(
            tree.root_label(),
            root.file_name().unwrap().to_string_lossy()
        );
        tree.activate().unwrap();

        assert_eq!(tree.root, parent);
        assert_eq!(
            tree.root_label(),
            parent.file_name().unwrap().to_string_lossy()
        );
        assert_eq!(tree.entries()[tree.selected()].path, root);
        assert_eq!(
            tree.entries()[tree.selected()].kind,
            TreeEntryKind::Directory
        );

        let mut tree = FileTree::new(root.clone());
        tree.collapse().unwrap();
        assert_eq!(tree.root, parent);
        assert_eq!(tree.entries()[tree.selected()].path, root);

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn labels_relative_directories_and_filesystem_roots() {
        let current = std::env::current_dir().unwrap();
        assert_eq!(
            directory_label(PathBuf::from(".").as_path()),
            current
                .file_name()
                .unwrap_or_else(|| current.as_os_str())
                .to_string_lossy()
        );
        assert_eq!(
            directory_label(PathBuf::from("chosen-alias").as_path()),
            "chosen-alias"
        );

        let mut root = current;
        while let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
        assert_eq!(directory_label(&root), root.to_string_lossy());
    }
}
