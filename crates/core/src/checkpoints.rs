//! File checkpoints for rewinding a session.
//!
//! Claude's `/rewind` restores the files a session edited, not just the
//! conversation. CodeRex's `/rewind` used to truncate the message history
//! only; this store gives it the file half.
//!
//! Every mutating edit tool snapshots the target file *before* it writes,
//! keyed by the tool call that is about to happen. `/rewind <n>` collects the
//! tool-call IDs of the messages it removed and asks the store to undo their
//! file effects: restore the earliest captured content per path, and delete
//! files the undone calls created.
//!
//! The store is per-session and self-cleaning: its blob directory is removed
//! when the session drops it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a checkpoint captured before a tool wrote a file.
#[derive(Debug)]
enum Snapshot {
    /// The file existed; its content lives in a blob under the store dir.
    Blob(PathBuf),
    /// The file did not exist; the following write created it.
    Absent,
}

#[derive(Debug)]
struct Entry {
    call_id: String,
    path: PathBuf,
    snapshot: Snapshot,
}

/// What [`CheckpointStore::restore`] did.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Files whose captured content was written back.
    pub restored: Vec<PathBuf>,
    /// Files the undone calls created, now removed.
    pub removed: Vec<PathBuf>,
    /// Paths that could not be restored (blob gone, or the write failed).
    pub failed: Vec<PathBuf>,
}

/// A per-session store of pre-edit file snapshots.
#[derive(Debug)]
pub struct CheckpointStore {
    dir: PathBuf,
    entries: Mutex<Vec<Entry>>,
    counter: AtomicU64,
}

impl CheckpointStore {
    /// Create a store under `root`, in a fresh per-session subdirectory.
    pub fn new(root: &Path) -> Result<Self, String> {
        let dir = root.join(format!("session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        Ok(Self {
            dir,
            entries: Mutex::new(Vec::new()),
            counter: AtomicU64::new(0),
        })
    }

    /// The directory this store keeps its blobs in (used by tests).
    #[cfg(test)]
    fn dir(&self) -> &Path {
        &self.dir
    }

    /// Capture `path` so a later [`restore`](Self::restore) can undo the write
    /// about to happen under `call_id`.
    pub fn snapshot(&self, call_id: &str, path: &Path) {
        let snapshot = if path.is_file() {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            let blob = self.dir.join(format!("{n}.blob"));
            match std::fs::copy(path, &blob) {
                Ok(_) => Snapshot::Blob(blob),
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        "file checkpoint: could not snapshot: {error}"
                    );
                    return;
                }
            }
        } else {
            Snapshot::Absent
        };
        if let Ok(mut entries) = self.entries.lock() {
            entries.push(Entry {
                call_id: call_id.to_string(),
                path: path.to_path_buf(),
                snapshot,
            });
        } else {
            tracing::warn!("file checkpoint: entries lock poisoned; snapshot lost");
        }
    }

    /// Record that `path` did not exist before the write named by `call_id`.
    ///
    /// A write tool opens the target before verifying its real path, so the
    /// file may exist (empty) by the time the snapshot is taken. This records
    /// the pre-write fact explicitly: restore removes the file instead of
    /// restoring an empty one.
    pub fn snapshot_absent(&self, call_id: &str, path: &Path) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.push(Entry {
                call_id: call_id.to_string(),
                path: path.to_path_buf(),
                snapshot: Snapshot::Absent,
            });
        } else {
            tracing::warn!("file checkpoint: entries lock poisoned; snapshot lost");
        }
    }

    /// Undo the file effects of the tool calls named by `call_ids`.
    ///
    /// Entries are applied earliest-first and the first entry wins per path:
    /// that is the content the path had before the first undone edit, which
    /// is what "restore to before these calls" means.
    pub fn restore(&self, call_ids: &HashSet<String>) -> RestoreReport {
        let mut report = RestoreReport::default();
        let Ok(entries) = self.entries.lock() else {
            return report;
        };
        let mut applied: HashSet<&Path> = HashSet::new();
        for entry in entries.iter() {
            if !call_ids.contains(&entry.call_id) {
                continue;
            }
            if !applied.insert(entry.path.as_path()) {
                continue;
            }
            match &entry.snapshot {
                Snapshot::Blob(blob) => match std::fs::read(blob) {
                    Ok(bytes) => match std::fs::write(&entry.path, bytes) {
                        Ok(()) => report.restored.push(entry.path.clone()),
                        Err(error) => {
                            tracing::warn!(
                                path = %entry.path.display(),
                                "file checkpoint: restore write failed: {error}"
                            );
                            report.failed.push(entry.path.clone());
                        }
                    },
                    Err(error) => {
                        tracing::warn!(
                            path = %entry.path.display(),
                            "file checkpoint: blob unreadable: {error}"
                        );
                        report.failed.push(entry.path.clone());
                    }
                },
                Snapshot::Absent => match std::fs::remove_file(&entry.path) {
                    Ok(()) => report.removed.push(entry.path.clone()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(
                            path = %entry.path.display(),
                            "file checkpoint: could not remove created file: {error}"
                        );
                        report.failed.push(entry.path.clone());
                    }
                },
            }
        }
        report
    }
}

impl Drop for CheckpointStore {
    fn drop(&mut self) {
        // Best-effort: the blobs only exist for rewinding this session.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> HashSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn restoring_undoes_edits_and_creations() {
        let root = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(root.path()).unwrap();

        // Edit an existing file.
        let existing = root.path().join("existing.txt");
        std::fs::write(&existing, "v1").unwrap();
        store.snapshot("call_a", &existing);
        std::fs::write(&existing, "v2").unwrap();

        // Create a new file.
        let created = root.path().join("created.txt");
        store.snapshot("call_b", &created);
        std::fs::write(&created, "brand new").unwrap();

        assert!(existing.exists() && created.exists());

        let report = store.restore(&ids(&["call_a", "call_b"]));
        assert_eq!(report.restored, vec![existing.clone()]);
        assert_eq!(report.removed, vec![created.clone()]);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "v1");
        assert!(!created.exists());
    }

    #[test]
    fn the_earliest_snapshot_per_path_wins() {
        let root = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(root.path()).unwrap();

        let file = root.path().join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        store.snapshot("call_1", &file);
        std::fs::write(&file, "v2").unwrap();
        store.snapshot("call_2", &file);
        std::fs::write(&file, "v3").unwrap();

        // Undoing both edits restores the content before the first of them.
        let report = store.restore(&ids(&["call_1", "call_2"]));
        assert_eq!(report.restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
    }

    #[test]
    fn unlisted_calls_are_not_undone() {
        let root = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(root.path()).unwrap();

        let file = root.path().join("f.txt");
        std::fs::write(&file, "v1").unwrap();
        store.snapshot("keep", &file);
        std::fs::write(&file, "v2").unwrap();
        store.snapshot("undo", &file);
        std::fs::write(&file, "v3").unwrap();

        let report = store.restore(&ids(&["undo"]));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
        assert_eq!(report.restored.len(), 1);
    }

    #[test]
    fn the_store_removes_its_blob_dir_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let dir = {
            let store = CheckpointStore::new(root.path()).unwrap();
            let dir = store.dir().to_path_buf();
            let file = root.path().join("f.txt");
            std::fs::write(&file, "v1").unwrap();
            store.snapshot("call", &file);
            assert!(dir.exists());
            dir
        };
        assert!(!dir.exists(), "dropping the store cleans its blobs");
    }
}
