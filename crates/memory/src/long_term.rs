//! Long-term memory store — file-based persistent memory with hierarchical keys.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A hierarchical memory key, e.g. `project/backend/auth`.
///
/// Keys use `/` as the segment separator and map to nested directories
/// on disk. A key like `a/b/c` is stored at `<base>/a/b/c.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryKey {
    segments: Vec<String>,
}

impl MemoryKey {
    /// Create a key from a slash-separated string.
    ///
    /// Empty segments and leading/trailing slashes are ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// use coderex_memory::long_term::MemoryKey;
    /// let key = MemoryKey::from_str("project/backend/auth");
    /// assert_eq!(key.segments(), vec!["project", "backend", "auth"]);
    /// ```
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        let segments: Vec<String> = s
            .split('/')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Self { segments }
    }

    /// Create a key from pre-split segments.
    #[must_use]
    pub fn from_segments(segments: Vec<String>) -> Self {
        Self { segments }
    }

    /// The key segments.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The key as a slash-separated string.
    #[must_use]
    pub fn to_path(&self) -> String {
        self.segments.join("/")
    }

    /// Returns true if `other` starts with this key's segments.
    ///
    /// `project/backend` is a prefix of `project/backend/auth`.
    #[must_use]
    pub fn is_prefix_of(&self, other: &MemoryKey) -> bool {
        if self.segments.is_empty() {
            return true;
        }
        if self.segments.len() > other.segments.len() {
            return false;
        }
        self.segments
            .iter()
            .zip(other.segments.iter())
            .all(|(a, b)| a == b)
    }

    /// Returns the parent key, if any.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.segments.len() <= 1 {
            return None;
        }
        Some(Self {
            segments: self.segments[..self.segments.len() - 1].to_vec(),
        })
    }

    /// Returns the last segment (the "name"), if any.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }
}

impl std::fmt::Display for MemoryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_path())
    }
}

/// A single long-term memory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// The topic/label for this memory (human-readable, flat).
    pub topic: String,
    /// Optional hierarchical key for structured retrieval.
    ///
    /// When present, the entry is stored under a nested directory path
    /// reflecting the key segments (e.g. `project/backend/auth`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
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
/// Entries with a hierarchical `key` are stored in nested directories
/// reflecting the key segments (e.g. `project/backend/auth.json`).
/// Entries with only a flat `topic` are stored at the top level.
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
        self.read_at_path(&path)
    }

    /// Read a memory by its hierarchical key.
    pub fn read_by_key(&self, key: &MemoryKey) -> std::io::Result<Option<MemoryEntry>> {
        let path = self.key_path(key);
        self.read_at_path(&path)
    }

    /// Write a memory topic. Creates new or updates existing.
    pub fn write(&self, topic: &str, content: &str) -> std::io::Result<()> {
        let path = self.entry_path(topic);
        self.write_at_path(&path, topic, None, content)
    }

    /// Write a memory with a hierarchical key.
    ///
    /// The `topic` is used as a human-readable label; the `key` determines
    /// the storage path. If `key` is empty, falls back to flat topic storage.
    pub fn write_with_key(
        &self,
        topic: &str,
        key: &MemoryKey,
        content: &str,
    ) -> std::io::Result<()> {
        let path = self.key_path(key);
        self.write_at_path(&path, topic, Some(key.to_path()), content)
    }

    /// Search memories by query string.
    ///
    /// Returns matches sorted by access_count (most accessed first).
    pub fn search(&self, query: &str) -> std::io::Result<Vec<MemoryEntry>> {
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        self.scan_dir(&self.base_path, &mut |entry| {
            if entry.topic.to_lowercase().contains(&query_lower)
                || entry.content.to_lowercase().contains(&query_lower)
                || entry
                    .key
                    .as_ref()
                    .map(|k| k.to_lowercase().contains(&query_lower))
                    .unwrap_or(false)
            {
                results.push(entry);
            }
        })?;
        results.sort_by_key(|m| std::cmp::Reverse(m.access_count));
        Ok(results)
    }

    /// Read all memories whose key starts with the given prefix.
    ///
    /// For example, `read_by_prefix(MemoryKey::from_str("project/backend"))`
    /// returns all entries with keys like `project/backend/auth`,
    /// `project/backend/routes`, etc.
    pub fn read_by_prefix(&self, prefix: &MemoryKey) -> std::io::Result<Vec<MemoryEntry>> {
        let mut results = Vec::new();
        let search_dir = if prefix.segments().is_empty() {
            self.base_path.clone()
        } else {
            self.base_path.join(prefix.to_path())
        };

        if !search_dir.exists() {
            return Ok(results);
        }

        self.scan_dir(&search_dir, &mut |entry| {
            if let Some(ref key) = entry.key {
                let entry_key = MemoryKey::from_str(key);
                if prefix.is_prefix_of(&entry_key) {
                    results.push(entry);
                }
            }
        })?;

        results.sort_by_key(|m| std::cmp::Reverse(m.access_count));
        Ok(results)
    }

    /// List all hierarchical keys in the store.
    ///
    /// Only returns keys for entries that were stored with an explicit
    /// hierarchical key via [`write_with_key`]. Flat topic entries are
    /// not included.
    pub fn list_keys(&self) -> std::io::Result<Vec<MemoryKey>> {
        let mut keys = Vec::new();
        self.scan_dir(&self.base_path, &mut |entry| {
            if let Some(ref key) = entry.key {
                keys.push(MemoryKey::from_str(key));
            }
        })?;
        Ok(keys)
    }

    // ─── Internal helpers ──────────────────────────────────────────────────

    fn read_at_path(&self, path: &Path) -> std::io::Result<Option<MemoryEntry>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        let mut entry: MemoryEntry = serde_json::from_str(&content).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        entry.access_count += 1;
        entry.updated_at = Utc::now();
        let updated = serde_json::to_string(&entry)
            .map_err(std::io::Error::other)?;
        std::fs::write(path, updated)?;
        Ok(Some(entry))
    }

    fn write_at_path(
        &self,
        path: &Path,
        topic: &str,
        key: Option<String>,
        content: &str,
    ) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let now = Utc::now();

        let entry = if path.exists() {
            if let Ok(Some(existing)) = self.read_raw_path(path) {
                MemoryEntry {
                    content: content.into(),
                    key: key.or(existing.key),
                    updated_at: now,
                    access_count: existing.access_count,
                    ..existing
                }
            } else {
                MemoryEntry {
                    topic: topic.into(),
                    key,
                    content: content.into(),
                    created_at: now,
                    updated_at: now,
                    access_count: 0,
                }
            }
        } else {
            MemoryEntry {
                topic: topic.into(),
                key,
                content: content.into(),
                created_at: now,
                updated_at: now,
                access_count: 0,
            }
        };

        let serialized = serde_json::to_string_pretty(&entry)
            .map_err(std::io::Error::other)?;
        std::fs::write(path, serialized)
    }

    fn read_raw_path(&self, path: &Path) -> std::io::Result<Option<MemoryEntry>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content).ok())
    }

    /// Convert a topic name to a safe flat filename.
    fn entry_path(&self, topic: &str) -> PathBuf {
        let safe = topic.replace(['/', '\\', ':', ' ', '.'], "_");
        self.base_path.join(format!("{safe}.json"))
    }

    /// Convert a hierarchical key to a nested path.
    fn key_path(&self, key: &MemoryKey) -> PathBuf {
        if key.segments().is_empty() {
            return self.base_path.join("_.json");
        }
        let mut path = self.base_path.clone();
        for (i, segment) in key.segments().iter().enumerate() {
            let safe = segment.replace(['/', '\\', ':', ' ', '.'], "_");
            if i == key.segments().len() - 1 {
                path = path.join(format!("{safe}.json"));
            } else {
                path = path.join(safe);
            }
        }
        path
    }

    /// Recursively scan a directory for `.json` memory entries.
    fn scan_dir(
        &self,
        dir: &Path,
        callback: &mut dyn FnMut(MemoryEntry),
    ) -> std::io::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                self.scan_dir(&path, callback)?;
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(memory) = serde_json::from_str::<MemoryEntry>(&content) {
                        callback(memory);
                    }
                }
            }
        }
        Ok(())
    }

    /// Recursively collect all hierarchical keys.
    fn collect_keys(
        &self,
        dir: &Path,
        current: &mut Vec<String>,
        keys: &mut Vec<MemoryKey>,
    ) -> std::io::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                current.push(name);
                self.collect_keys(&path, current, keys)?;
                current.pop();
            } else if name.ends_with(".json") {
                let stem = name.strip_suffix(".json").unwrap_or(&name);
                let mut segments = current.clone();
                segments.push(stem.to_string());
                keys.push(MemoryKey::from_segments(segments));
            }
        }
        Ok(())
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

    // ─── Hierarchical key tests ────────────────────────────────────────────

    #[test]
    fn memory_key_parsing() {
        let key = MemoryKey::from_str("project/backend/auth");
        assert_eq!(key.segments(), vec!["project", "backend", "auth"]);
        assert_eq!(key.to_path(), "project/backend/auth");
        assert_eq!(key.name(), Some("auth"));
        assert_eq!(key.parent().unwrap().to_path(), "project/backend");
    }

    #[test]
    fn memory_key_prefix_matching() {
        let prefix = MemoryKey::from_str("project/backend");
        let child = MemoryKey::from_str("project/backend/auth");
        let sibling = MemoryKey::from_str("project/frontend/auth");
        let parent = MemoryKey::from_str("project");

        assert!(prefix.is_prefix_of(&child));
        assert!(!prefix.is_prefix_of(&sibling));
        assert!(parent.is_prefix_of(&child));
        assert!(!child.is_prefix_of(&prefix));
    }

    #[test]
    fn write_with_key_creates_nested_path() {
        let (store, tmp) = temp_store();
        let key = MemoryKey::from_str("project/backend/auth");
        store
            .write_with_key("Auth module", &key, "JWT-based authentication")
            .unwrap();

        // Should be stored at <base>/project/backend/auth.json
        let expected = tmp.path().join("project/backend/auth.json");
        assert!(expected.exists());

        let entry = store.read_by_key(&key).unwrap().unwrap();
        assert_eq!(entry.topic, "Auth module");
        assert_eq!(entry.key, Some("project/backend/auth".into()));
        assert_eq!(entry.content, "JWT-based authentication");
    }

    #[test]
    fn read_by_prefix_returns_matching_entries() {
        let (store, _tmp) = temp_store();
        store
            .write_with_key(
                "Auth",
                &MemoryKey::from_str("project/backend/auth"),
                "JWT auth",
            )
            .unwrap();
        store
            .write_with_key(
                "Routes",
                &MemoryKey::from_str("project/backend/routes"),
                "REST routes",
            )
            .unwrap();
        store
            .write_with_key(
                "Components",
                &MemoryKey::from_str("project/frontend/components"),
                "React components",
            )
            .unwrap();
        store
            .write("flat-topic", "no hierarchy")
            .unwrap();

        let backend = store
            .read_by_prefix(&MemoryKey::from_str("project/backend"))
            .unwrap();
        assert_eq!(backend.len(), 2);
        assert!(backend.iter().any(|m| m.topic == "Auth"));
        assert!(backend.iter().any(|m| m.topic == "Routes"));

        let project = store
            .read_by_prefix(&MemoryKey::from_str("project"))
            .unwrap();
        assert_eq!(project.len(), 3);
    }

    #[test]
    fn list_keys_returns_all_hierarchical_keys() {
        let (store, _tmp) = temp_store();
        store
            .write_with_key("A", &MemoryKey::from_str("x/y"), "content a")
            .unwrap();
        store
            .write_with_key("B", &MemoryKey::from_str("x/z"), "content b")
            .unwrap();
        store.write("flat", "no key").unwrap();

        let keys = store.list_keys().unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&MemoryKey::from_str("x/y")));
        assert!(keys.contains(&MemoryKey::from_str("x/z")));
    }

    #[test]
    fn search_includes_key_field() {
        let (store, _tmp) = temp_store();
        store
            .write_with_key("Auth", &MemoryKey::from_str("project/backend/auth"), "JWT")
            .unwrap();

        let results = store.search("backend").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].topic, "Auth");
    }

    #[test]
    fn hierarchical_and_flat_coexist() {
        let (store, _tmp) = temp_store();
        store.write("flat-topic", "flat content").unwrap();
        store
            .write_with_key("Hier", &MemoryKey::from_str("a/b"), "hier content")
            .unwrap();

        let all = store.search("content").unwrap();
        assert_eq!(all.len(), 2);
    }
}
