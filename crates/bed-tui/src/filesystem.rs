//! Bounded filesystem notifications and background directory scans.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashSet,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
};

const WATCH_QUEUE_CAPACITY: usize = 1024;
const SCAN_QUEUE_CAPACITY: usize = 64;

pub(crate) struct FileSystemWatcher {
    watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
    overflowed: Arc<AtomicBool>,
    watched: HashSet<PathBuf>,
}

impl fmt::Debug for FileSystemWatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileSystemWatcher")
            .field("watched", &self.watched)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub(crate) struct WatchBatch {
    pub(crate) paths: HashSet<PathBuf>,
    pub(crate) overflowed: bool,
    pub(crate) errors: Vec<String>,
}

impl FileSystemWatcher {
    pub(crate) fn new() -> notify::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(WATCH_QUEUE_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = Arc::clone(&overflowed);
        let watcher = notify::recommended_watcher(move |event| {
            if let Err(error) = sender.try_send(event) {
                match error {
                    TrySendError::Full(_) => callback_overflowed.store(true, Ordering::Release),
                    TrySendError::Disconnected(_) => {}
                }
            }
        })?;
        Ok(Self {
            watcher,
            receiver,
            overflowed,
            watched: HashSet::new(),
        })
    }

    pub(crate) fn sync(&mut self, directories: HashSet<PathBuf>) -> Vec<String> {
        let directories: HashSet<_> = directories
            .into_iter()
            .filter_map(|path| absolute_path(&path).ok())
            .collect();
        let removed: Vec<_> = self.watched.difference(&directories).cloned().collect();
        let added: Vec<_> = directories.difference(&self.watched).cloned().collect();
        let mut errors = Vec::new();

        for path in removed {
            if let Err(error) = self.watcher.unwatch(&path) {
                errors.push(format!(
                    "failed to stop watching {}: {error}",
                    path.display()
                ));
            }
            self.watched.remove(&path);
        }
        for path in added {
            match self.watcher.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(path);
                }
                Err(error) => errors.push(format!("failed to watch {}: {error}", path.display())),
            }
        }
        errors
    }

    pub(crate) fn drain(&self) -> WatchBatch {
        let mut batch = WatchBatch {
            overflowed: self.overflowed.swap(false, Ordering::AcqRel),
            ..WatchBatch::default()
        };
        loop {
            match self.receiver.try_recv() {
                Ok(Ok(event)) => {
                    if matches!(event.kind, EventKind::Access(_)) {
                        continue;
                    }
                    if event.paths.is_empty() {
                        batch.overflowed = true;
                    }
                    for path in event.paths {
                        match absolute_path(&path) {
                            Ok(path) => {
                                batch.paths.insert(path);
                            }
                            Err(error) => batch.errors.push(error.to_string()),
                        }
                    }
                }
                Ok(Err(error)) => {
                    batch.overflowed = true;
                    batch.errors.push(error.to_string());
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        batch
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ScanKey {
    pub(crate) tab: u64,
    pub(crate) tree_revision: u64,
    pub(crate) directory_revision: u64,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScannedEntry {
    pub(crate) path: PathBuf,
    pub(crate) is_directory: bool,
}

#[derive(Debug)]
pub(crate) struct ScanResult {
    pub(crate) key: ScanKey,
    pub(crate) entries: io::Result<Vec<ScannedEntry>>,
}

#[derive(Debug)]
pub(crate) struct DirectoryScanner {
    sender: SyncSender<ScanKey>,
    receiver: Receiver<ScanResult>,
    pending: HashSet<ScanKey>,
}

impl DirectoryScanner {
    pub(crate) fn new() -> io::Result<Self> {
        let (request_sender, request_receiver) = mpsc::sync_channel(SCAN_QUEUE_CAPACITY);
        let (result_sender, result_receiver) = mpsc::sync_channel(SCAN_QUEUE_CAPACITY);
        thread::Builder::new()
            .name(String::from("bed-directory-scan"))
            .spawn(move || scan_worker(request_receiver, result_sender))?;
        Ok(Self {
            sender: request_sender,
            receiver: result_receiver,
            pending: HashSet::new(),
        })
    }

    pub(crate) fn request(&mut self, key: ScanKey) -> bool {
        if self.pending.contains(&key) {
            return true;
        }
        match self.sender.try_send(key.clone()) {
            Ok(()) => {
                self.pending.insert(key);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    pub(crate) fn drain(&mut self) -> Vec<ScanResult> {
        let mut results = Vec::new();
        while let Ok(result) = self.receiver.try_recv() {
            self.pending.remove(&result.key);
            results.push(result);
        }
        results
    }
}

fn scan_worker(receiver: Receiver<ScanKey>, sender: SyncSender<ScanResult>) {
    while let Ok(key) = receiver.recv() {
        let entries = scan_directory(&key.path);
        if sender.send(ScanResult { key, entries }).is_err() {
            break;
        }
    }
}

fn scan_directory(directory: &Path) -> io::Result<Vec<ScannedEntry>> {
    let mut entries = fs::read_dir(directory)?
        .map(|entry| {
            let entry = entry?;
            Ok(ScannedEntry {
                path: entry.path(),
                is_directory: entry.file_type()?.is_dir(),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_cached_key(|entry| {
        (
            !entry.is_directory,
            entry
                .path
                .file_name()
                .map(|name| name.to_os_string())
                .unwrap_or_default(),
        )
    });
    Ok(entries)
}

pub(crate) fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    std::path::absolute(path)
}
