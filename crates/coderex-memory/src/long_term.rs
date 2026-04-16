//! Long-term memory store — file-based persistent memory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single long-term memory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// The topic/label for this memory.
    pub topic: String,
    /// The memory content.
    pub content: String,
    /// When this memory was created.
    pub created_at: DateTime<Utc>,
    /// When this memory was last updated.
    pub updated_at: DateTime<Utc>,
    /// How many times this memory has been accessed.
    pub access_count: u64,
}

/// Long-term memory stored under `<base_path>/`.
///
/// Each topic is a JSON file named after the sanitized topic.
#[derive(Debug)]
pub struct LongTermStore {
    base_path: PathBuf,
}

impl LongTermStore {
    /// Create a new long-term memory store.
    #[must_use]
    pub const fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    /// Read a memory topic. Increments access count on read.
    pub fn read(&self, topic: &str) -> std::io::Result<Option<MemoryEntry>> {
        let path = self.entry_path(topic);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let mut entry: MemoryEntry = serde_json::from_str(&content).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        entry.access_count += 1;
        entry.updated_at = Utc::now();
        let updated = serde_json::to_string(&entry)
            .map_err(std::io::Error::other)?;
        std::fs::write(&path, updated)?;
        Ok(Some(entry))
    }

    /// Write a memory topic. Creates new or updates existing.
    pub fn write(&self, topic: &str, content: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.base_path)?;
        let path = self.entry_path(topic);
        let now = Utc::now();

        let entry = if let Some(existing) = self.read_raw(topic)? {
            MemoryEntry {
                content: content.into(),
                updated_at: now,
                access_count: existing.access_count,
                ..existing
            }
        } else {
            MemoryEntry {
                topic: topic.into(),
                content: content.into(),
                created_at: now,
                updated_at: now,
                access_count: 0,
            }
        };

        let serialized = serde_json::to_string_pretty(&entry)
            .map_err(std::io::Error::other)?;
        std::fs::write(&path, serialized)
    }

    /// Search memories by query string.
    ///
    /// Returns matches sorted by access_count (most accessed first).
    pub fn search(&self, query: &str) -> std::io::Result<Vec<MemoryEntry>> {
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        if !self.base_path.exists() {
            return Ok(results);
        }

        for entry in std::fs::read_dir(&self.base_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = std::fs::read_to_string(&path)?;
            if let Ok(memory) = serde_json::from_str::<MemoryEntry>(&content) {
                if memory.topic.to_lowercase().contains(&query_lower)
                    || memory.content.to_lowercase().contains(&query_lower)
                {
                    results.push(memory);
                }
            }
        }

        results.sort_by_key(|m| std::cmp::Reverse(m.access_count));
        Ok(results)
    }

    /// Read raw entry without incrementing access count.
    fn read_raw(&self, topic: &str) -> std::io::Result<Option<MemoryEntry>> {
        let path = self.entry_path(topic);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&content).ok())
    }

    /// Convert a topic name to a safe filename.
    fn entry_path(&self, topic: &str) -> PathBuf {
        let safe = topic.replace(['/', '\\', ':', ' ', '.'], "_");
        self.base_path.join(format!("{safe}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (LongTermStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = LongTermStore::new(tmp.path().to_path_buf());
        (store, tmp)
    }

    #[test]
    fn write_then_read_returns_same_content() {
        let (store, _tmp) = temp_store();
        store.write("project-structure", "The project uses Cargo workspace").unwrap();

        let entry = store.read("project-structure").unwrap().unwrap();
        assert_eq!(entry.topic, "project-structure");
        assert_eq!(entry.content, "The project uses Cargo workspace");
        assert_eq!(entry.access_count, 1);
    }

    #[test]
    fn write_to_existing_topic_updates_content() {
        let (store, _tmp) = temp_store();
        store.write("topic-a", "first content").unwrap();
        store.write("topic-a", "updated content").unwrap();

        let entry = store.read("topic-a").unwrap().unwrap();
        assert_eq!(entry.content, "updated content");
    }

    #[test]
    fn read_nonexistent_topic_returns_none() {
        let (store, _tmp) = temp_store();
        let result = store.read("does-not-exist").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn search_finds_matching_topics() {
        let (store, _tmp) = temp_store();
        store.write("rust-patterns", "Use iterators").unwrap();
        store.write("python-patterns", "Use generators").unwrap();
        store.write("deployment", "Deploy to production").unwrap();

        let results = store.search("patterns").unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|m| m.topic == "rust-patterns"));
        assert!(results.iter().any(|m| m.topic == "python-patterns"));
    }

    #[test]
    fn search_results_sorted_by_access_count() {
        let (store, _tmp) = temp_store();
        store.write("topic-x", "content x").unwrap();
        store.write("topic-y", "content y").unwrap();

        // Access topic-y more times
        for _ in 0..5 {
            store.read("topic-y").unwrap();
        }
        store.read("topic-x").unwrap();

        let results = store.search("content").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].topic, "topic-y");
        assert_eq!(results[1].topic, "topic-x");
    }
}
