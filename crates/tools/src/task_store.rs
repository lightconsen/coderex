//! TaskStore — in-memory task registry shared via `ToolContext.extra`.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// A task record stored in the TaskStore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Unique task ID.
    pub id: String,
    /// Short subject/title.
    pub subject: String,
    /// Detailed description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Current status.
    pub status: TaskStatus,
    /// Present continuous form shown in spinner (e.g. "Running tests").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_form: String,
    /// Path to a file containing task output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file_path: Option<String>,
    /// Task creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Owner (agent/shell ID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Task IDs that this task blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    /// Task IDs that block this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// Arbitrary metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not yet started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Done.
    Completed,
    /// Removed from the store.
    Deleted,
}

impl Default for TaskStatus {
    fn default() -> Self {
        Self::Pending
    }
}

impl TaskRecord {
    /// Create a new task record.
    pub fn new(id: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            subject: subject.into(),
            description: String::new(),
            status: TaskStatus::Pending,
            active_form: String::new(),
            output_file_path: None,
            created_at: Utc::now(),
            owner: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: None,
        }
    }
}

/// Shared storage for tasks.
#[derive(Debug, Default)]
pub struct TaskStore {
    tasks: DashMap<String, TaskRecord>,
    counter: Mutex<u64>,
    storage_path: Mutex<Option<PathBuf>>,
}

impl TaskStore {
    /// Create an empty task store.
    pub fn new() -> Self {
        Self {
            tasks: DashMap::new(),
            counter: Mutex::new(0),
            storage_path: Mutex::new(None),
        }
    }

    /// Set the filesystem path used for `load` / `save`.
    pub fn with_storage(self, path: PathBuf) -> Self {
        *self.storage_path.lock() = Some(path);
        self
    }

    /// Load tasks from a JSONL file if the storage path is set and the file exists.
    pub fn load(&self) -> Result<(), String> {
        let guard = self.storage_path.lock();
        let path = guard.as_ref().ok_or_else(|| "no storage path set".to_string())?;
        if !path.exists() {
            return Ok(());
        }
        let file = fs::File::open(path).map_err(|e| format!("failed to open: {e}"))?;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let line = line.map_err(|e| format!("failed to read line: {e}"))?;
            if line.trim().is_empty() {
                continue;
            }
            let record: TaskRecord =
                serde_json::from_str(&line).map_err(|e| format!("failed to parse: {e}"))?;
            self.tasks.insert(record.id.clone(), record);
        }
        Ok(())
    }

    /// Persist all non-deleted tasks to the JSONL file.
    pub fn save(&self) -> Result<(), String> {
        let guard = self.storage_path.lock();
        let path = guard.as_ref().ok_or_else(|| "no storage path set".to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create dir: {e}"))?;
        }
        let mut file = fs::File::create(path).map_err(|e| format!("failed to create file: {e}"))?;
        for record in self.tasks.iter() {
            if record.value().status == TaskStatus::Deleted {
                continue;
            }
            let line = serde_json::to_string(record.value())
                .map_err(|e| format!("failed to serialize: {e}"))?;
            writeln!(file, "{line}").map_err(|e| format!("failed to write: {e}"))?;
        }
        Ok(())
    }

    /// Generate a unique task ID.
    pub fn next_id(&self) -> String {
        let mut c = self.counter.lock();
        *c += 1;
        c.to_string()
    }

    /// Create a new task.
    pub fn create(&self, record: TaskRecord) {
        self.tasks.insert(record.id.clone(), record);
        let _ = self.save();
    }

    /// Get a task by ID.
    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        self.tasks.get(id).map(|r| r.value().clone())
    }

    /// List all tasks, optionally filtered by status.
    pub fn list(&self, status_filter: Option<TaskStatus>) -> Vec<TaskRecord> {
        self.tasks
            .iter()
            .filter(|r| {
                status_filter.map_or(true, |s| r.value().status == s)
            })
            .map(|r| r.value().clone())
            .collect()
    }

    /// Update a task's fields. Only non-None fields are updated.
    pub fn update(
        &self,
        id: &str,
        status: Option<TaskStatus>,
        subject: Option<String>,
        description: Option<String>,
        active_form: Option<String>,
    ) -> bool {
        let ok = if let Some(mut r) = self.tasks.get_mut(id) {
            if let Some(s) = status {
                r.status = s;
            }
            if let Some(s) = subject {
                r.subject = s;
            }
            if let Some(d) = description {
                r.description = d;
            }
            if let Some(a) = active_form {
                r.active_form = a;
            }
            true
        } else {
            false
        };
        if ok {
            let _ = self.save();
        }
        ok
    }

    /// Delete a task by marking it as deleted.
    pub fn delete(&self, id: &str) -> bool {
        let ok = if let Some(mut r) = self.tasks.get_mut(id) {
            r.status = TaskStatus::Deleted;
            true
        } else {
            false
        };
        if ok {
            let _ = self.save();
        }
        ok
    }

    /// Set the output file path for a task.
    pub fn set_output_file(&self, id: &str, path: &str) {
        if let Some(mut r) = self.tasks.get_mut(id) {
            r.output_file_path = Some(path.to_string());
        }
        let _ = self.save();
    }

    /// Set the owner for a task.
    pub fn set_owner(&self, id: &str, owner: &str) {
        if let Some(mut r) = self.tasks.get_mut(id) {
            r.owner = Some(owner.to_string());
        }
        let _ = self.save();
    }
}

/// Key used in `ToolContext.extra` to access the TaskStore.
pub const TASK_STORE_KEY: &str = "task_store";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_store_new_is_empty() {
        let store = TaskStore::new();
        assert!(store.list(None).is_empty());
    }

    #[test]
    fn task_store_create_and_get() {
        let store = TaskStore::new();
        let record = TaskRecord::new("1", "test task");
        store.create(record);
        let got = store.get("1").unwrap();
        assert_eq!(got.subject, "test task");
        assert!(matches!(got.status, TaskStatus::Pending));
    }

    #[test]
    fn task_store_list_with_filter() {
        let store = TaskStore::new();
        let mut r1 = TaskRecord::new("1", "pending task");
        r1.status = TaskStatus::Pending;
        let mut r2 = TaskRecord::new("2", "done task");
        r2.status = TaskStatus::Completed;
        store.create(r1);
        store.create(r2);

        let all = store.list(None);
        assert_eq!(all.len(), 2);

        let completed = store.list(Some(TaskStatus::Completed));
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].subject, "done task");
    }

    #[test]
    fn task_store_update() {
        let store = TaskStore::new();
        store.create(TaskRecord::new("1", "old"));
        let ok = store.update("1", Some(TaskStatus::InProgress), Some("new".to_string()), None, None);
        assert!(ok);
        let got = store.get("1").unwrap();
        assert_eq!(got.subject, "new");
        assert!(matches!(got.status, TaskStatus::InProgress));
    }

    #[test]
    fn task_store_update_nonexistent() {
        let store = TaskStore::new();
        let ok = store.update("999", None, None, None, None);
        assert!(!ok);
    }

    #[test]
    fn task_store_next_id() {
        let store = TaskStore::new();
        let id1 = store.next_id();
        let id2 = store.next_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn task_store_output_file() {
        let store = TaskStore::new();
        store.create(TaskRecord::new("1", "test"));
        store.set_output_file("1", "/tmp/out.txt");
        let got = store.get("1").unwrap();
        assert_eq!(got.output_file_path, Some("/tmp/out.txt".into()));
    }
}
