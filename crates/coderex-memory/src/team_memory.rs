//! Team memory sync — repo-dimensioned memory with checksum-based incremental sync.
//!
//! Each repository has a shared memory namespace. Team members can upsert memories,
//! and sync uses SHA-256 checksums to detect changes, transferring only deltas.
//! Deletes are local-only and do not propagate to other clients.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

/// Checksum of memory content for incremental sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryChecksum(pub [u8; 32]);

impl MemoryChecksum {
    /// Compute checksum from content bytes.
    #[must_use]
    pub fn from_content(content: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        Self(hasher.finalize().into())
    }

    /// Hex-encoded checksum string.
    #[must_use]
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// A team memory entry with checksum and tombstone support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemoryEntry {
    /// Memory topic/label.
    pub topic: String,
    /// Memory content.
    pub content: String,
    /// Checksum of content for delta detection.
    pub checksum: MemoryChecksum,
    /// When this entry was created.
    pub created_at: DateTime<Utc>,
    /// When this entry was last updated.
    pub updated_at: DateTime<Utc>,
    /// Whether this entry has been locally deleted.
    /// Deleted entries are excluded from sync propagation.
    pub tombstoned: bool,
}

/// Result of a sync operation.
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// Entries that need to be sent to the server (local changes).
    pub to_send: Vec<TeamMemoryEntry>,
    /// Entries that need to be received from the server (remote changes).
    pub to_receive: Vec<TeamMemoryEntry>,
    /// Total entries in local store.
    pub local_count: usize,
    /// Total entries the server has.
    pub remote_count: usize,
}

/// In-memory representation of remote state for sync comparison.
#[derive(Debug, Clone, Default)]
pub struct RemoteState {
    /// Map of topic -> (checksum, tombstoned) from the server.
    pub entries: HashMap<String, (MemoryChecksum, bool, DateTime<Utc>)>,
}

impl RemoteState {
    /// Add a remote entry to the state.
    pub fn add(&mut self, topic: String, checksum: MemoryChecksum, tombstoned: bool, updated_at: DateTime<Utc>) {
        self.entries.insert(topic, (checksum, tombstoned, updated_at));
    }
}

/// Team memory sync service for a single repository.
///
/// Memories are stored in `<repo_memory_dir>/<org_id>/<topic>.json`.
/// Sync uses checksums to detect changes and only transfers deltas.
#[derive(Debug)]
pub struct TeamMemorySyncService {
    /// Base directory for team memories (typically `<repo>/.coderex/team-memory/`).
    pub memory_dir: PathBuf,
    /// Repo identifier.
    pub repo_id: String,
    /// Optional organization ID for org-scoped isolation.
    /// When set, memories are stored under a subdirectory for this org.
    pub org_id: Option<String>,
}

impl TeamMemorySyncService {
    /// Create a new team memory sync service.
    #[must_use]
    pub fn new(repo_id: String, memory_dir: PathBuf) -> Self {
        Self { repo_id, memory_dir, org_id: None }
    }

    /// Create a new team memory sync service scoped to an organization.
    #[must_use]
    pub fn new_with_org(repo_id: String, memory_dir: PathBuf, org_id: String) -> Self {
        Self {
            repo_id,
            memory_dir,
            org_id: Some(org_id),
        }
    }

    /// Set the organization scope for this service.
    #[must_use]
    pub fn with_org(mut self, org_id: String) -> Self {
        self.org_id = Some(org_id);
        self
    }

    /// Get the effective memory directory (includes org subdirectory if scoped).
    fn effective_memory_dir(&self) -> PathBuf {
        match &self.org_id {
            Some(org) => {
                let safe_org = org.replace(['/', '\\', ':', ' '], "_");
                self.memory_dir.join(&safe_org)
            }
            None => self.memory_dir.clone(),
        }
    }

    /// Upsert a memory entry.
    ///
    /// Creates a new entry or updates an existing one.
    /// Computes checksum from the new content.
    pub fn upsert(&self, topic: &str, content: &str) -> std::io::Result<TeamMemoryEntry> {
        let mem_dir = self.effective_memory_dir();
        std::fs::create_dir_all(&mem_dir)?;

        let checksum = MemoryChecksum::from_content(content);
        let now = Utc::now();

        let entry = if let Some(existing) = self.read_entry_raw(topic)? {
            TeamMemoryEntry {
                content: content.to_string(),
                checksum,
                updated_at: now,
                tombstoned: false, // Upsert un-tombstones
                ..existing
            }
        } else {
            TeamMemoryEntry {
                topic: topic.to_string(),
                content: content.to_string(),
                checksum,
                created_at: now,
                updated_at: now,
                tombstoned: false,
            }
        };

        self.write_entry(&entry)?;
        Ok(entry)
    }

    /// Read a memory entry.
    pub fn read(&self, topic: &str) -> std::io::Result<Option<TeamMemoryEntry>> {
        let path = self.entry_path(topic);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let entry: TeamMemoryEntry = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(if entry.tombstoned { None } else { Some(entry) })
    }

    /// Delete a memory entry locally.
    ///
    /// Marks the entry as tombstoned rather than removing it.
    /// Tombstoned entries are NOT included in sync to_send (deletion does not propagate).
    pub fn delete(&self, topic: &str) -> std::io::Result<()> {
        if let Some(mut entry) = self.read_entry_raw(topic)? {
            entry.tombstoned = true;
            self.write_entry(&entry)
        } else {
            Ok(())
        }
    }

    /// List all non-tombstoned entries.
    pub fn list(&self) -> std::io::Result<Vec<TeamMemoryEntry>> {
        let mem_dir = self.effective_memory_dir();
        if !mem_dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&mem_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = std::fs::read_to_string(&path)?;
            if let Ok(mem) = serde_json::from_str::<TeamMemoryEntry>(&content) {
                if !mem.tombstoned {
                    entries.push(mem);
                }
            }
        }
        Ok(entries)
    }

    /// Compute the sync delta between local and remote state.
    ///
    /// - Local entries with a different checksum than remote → to_send
    /// - Remote entries that don't exist locally (or differ) → to_receive
    /// - Tombstoned local entries are excluded from to_send (deletions don't propagate)
    #[must_use]
    pub fn compute_sync_delta(&self, remote: &RemoteState) -> SyncResult {
        let local_entries = self.list().unwrap_or_default();
        let local_map: HashMap<String, &TeamMemoryEntry> = local_entries
            .iter()
            .map(|e| (e.topic.clone(), e))
            .collect();

        // Find local entries that need to be sent (different checksum or new)
        let mut to_send = Vec::new();
        for entry in &local_entries {
            if let Some((remote_checksum, _, _)) = remote.entries.get(&entry.topic) {
                if entry.checksum != *remote_checksum {
                    to_send.push(entry.clone());
                }
            } else {
                // New entry not on server
                to_send.push(entry.clone());
            }
        }

        // Find remote entries that need to be received
        let mut to_receive = Vec::new();
        for (topic, (checksum, tombstoned, updated_at)) in remote.entries.iter() {
            if *tombstoned {
                continue;
            }
            match local_map.get(topic.as_str()) {
                None => {
                    to_receive.push(TeamMemoryEntry {
                        topic: topic.clone(),
                        content: String::new(),
                        checksum: *checksum,
                        created_at: *updated_at,
                        updated_at: *updated_at,
                        tombstoned: false,
                    });
                }
                Some(local) => {
                    if local.checksum != *checksum {
                        if *updated_at > local.updated_at {
                            to_receive.push(TeamMemoryEntry {
                                topic: topic.clone(),
                                content: String::new(),
                                checksum: *checksum,
                                created_at: local.created_at,
                                updated_at: *updated_at,
                                tombstoned: false,
                            });
                        }
                    }
                }
            }
        }

        SyncResult {
            to_send,
            to_receive,
            local_count: local_entries.len(),
            remote_count: remote.entries.len(),
        }
    }

    /// Apply received remote entries to the local store.
    pub fn apply_remote(&self, entries: Vec<TeamMemoryEntry>) -> std::io::Result<usize> {
        let mut applied = 0;
        for entry in entries {
            // Only apply if remote is newer or local doesn't exist
            if let Some(local) = self.read_entry_raw(&entry.topic)? {
                if entry.updated_at > local.updated_at {
                    self.write_entry(&entry)?;
                    applied += 1;
                }
            } else {
                self.write_entry(&entry)?;
                applied += 1;
            }
        }
        Ok(applied)
    }

    /// Read entry including tombstoned ones.
    fn read_entry_raw(&self, topic: &str) -> std::io::Result<Option<TeamMemoryEntry>> {
        let path = self.entry_path(topic);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Write an entry to disk.
    fn write_entry(&self, entry: &TeamMemoryEntry) -> std::io::Result<()> {
        let path = self.entry_path(&entry.topic);
        let serialized = serde_json::to_string_pretty(entry)
            .map_err(std::io::Error::other)?;
        std::fs::write(&path, serialized)
    }

    /// Convert topic to a safe filename.
    fn entry_path(&self, topic: &str) -> PathBuf {
        let safe = topic.replace(['/', '\\', ':', ' '], "_");
        self.effective_memory_dir().join(format!("{safe}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_service() -> (TeamMemorySyncService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let svc = TeamMemorySyncService::new("repo1".to_string(), tmp.path().to_path_buf());
        (svc, tmp)
    }

    #[test]
    fn upsert_creates_new_entry() {
        let (svc, _tmp) = temp_service();
        let entry = svc.upsert("architecture", "Uses layered architecture").unwrap();
        assert_eq!(entry.topic, "architecture");
        assert_eq!(entry.content, "Uses layered architecture");
        assert!(!entry.tombstoned);
    }

    #[test]
    fn upsert_updates_existing_entry() {
        let (svc, _tmp) = temp_service();
        svc.upsert("db", "PostgreSQL").unwrap();
        let entry = svc.upsert("db", "PostgreSQL 15").unwrap();
        assert_eq!(entry.content, "PostgreSQL 15");
    }

    #[test]
    fn upsert_recomputes_checksum() {
        let (svc, _tmp) = temp_service();
        let e1 = svc.upsert("key", "value1").unwrap();
        let e2 = svc.upsert("key", "value2").unwrap();
        assert_ne!(e1.checksum.to_hex(), e2.checksum.to_hex());
    }

    #[test]
    fn delete_tombstones_entry() {
        let (svc, _tmp) = temp_service();
        svc.upsert("temp", "something").unwrap();
        svc.delete("temp").unwrap();
        // read() returns None for tombstoned
        assert!(svc.read("temp").unwrap().is_none());
        // But raw read still exists
        assert!(svc.read_entry_raw("temp").unwrap().unwrap().tombstoned);
    }

    #[test]
    fn delete_does_not_propagate_in_sync() {
        let (svc, _tmp) = temp_service();
        svc.upsert("keep", "val").unwrap();
        svc.upsert("remove", "val").unwrap();
        svc.delete("remove").unwrap();

        let remote = RemoteState::default();
        let delta = svc.compute_sync_delta(&remote);
        // Tombstoned entry should NOT be in to_send
        assert!(!delta.to_send.iter().any(|e| e.topic == "remove"));
        // Non-deleted entry should be in to_send (it's new to server)
        assert!(delta.to_send.iter().any(|e| e.topic == "keep"));
    }

    #[test]
    fn list_excludes_tombstoned() {
        let (svc, _tmp) = temp_service();
        svc.upsert("a", "1").unwrap();
        svc.upsert("b", "2").unwrap();
        svc.delete("b").unwrap();

        let listed = svc.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].topic, "a");
    }

    #[test]
    fn checksum_is_deterministic() {
        let c1 = MemoryChecksum::from_content("hello");
        let c2 = MemoryChecksum::from_content("hello");
        assert_eq!(c1.to_hex(), c2.to_hex());

        let c3 = MemoryChecksum::from_content("world");
        assert_ne!(c1.to_hex(), c3.to_hex());
    }

    #[test]
    fn sync_delta_sends_new_local_entries() {
        let (svc, _tmp) = temp_service();
        svc.upsert("local-only", "content").unwrap();

        let remote = RemoteState::default();
        let delta = svc.compute_sync_delta(&remote);

        assert_eq!(delta.to_send.len(), 1);
        assert_eq!(delta.to_send[0].topic, "local-only");
        assert!(delta.to_receive.is_empty());
    }

    #[test]
    fn sync_delta_sends_changed_entries() {
        let (svc, _tmp) = temp_service();
        svc.upsert("shared", "v1").unwrap();

        let checksum_different = MemoryChecksum::from_content("v2");
        let mut remote = RemoteState::default();
        let now = Utc::now();
        remote.add("shared".to_string(), checksum_different, false, now + chrono::Duration::seconds(10));

        let delta = svc.compute_sync_delta(&remote);

        // Remote is newer and different → to_receive
        assert_eq!(delta.to_receive.len(), 1);
        assert_eq!(delta.to_receive[0].topic, "shared");
    }

    #[test]
    fn sync_delta_no_change_when_checksums_match() {
        let (svc, _tmp) = temp_service();
        let entry = svc.upsert("same", "content").unwrap();

        let mut remote = RemoteState::default();
        remote.add("same".to_string(), entry.checksum, false, entry.updated_at);

        let delta = svc.compute_sync_delta(&remote);

        assert!(delta.to_send.is_empty());
        assert!(delta.to_receive.is_empty());
    }

    #[test]
    fn apply_remote_adds_new_entries() {
        let (svc, _tmp) = temp_service();
        let remote_entries = vec![
            TeamMemoryEntry {
                topic: "remote-a".to_string(),
                content: "from remote".to_string(),
                checksum: MemoryChecksum::from_content("from remote"),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                tombstoned: false,
            },
        ];

        let count = svc.apply_remote(remote_entries).unwrap();
        assert_eq!(count, 1);

        let local = svc.read("remote-a").unwrap().unwrap();
        assert_eq!(local.content, "from remote");
    }

    #[test]
    fn apply_remote_skips_older_entries() {
        let (svc, _tmp) = temp_service();
        svc.upsert("local", "newer content").unwrap();

        let older = Utc::now() - chrono::Duration::hours(1);
        let remote_entries = vec![
            TeamMemoryEntry {
                topic: "local".to_string(),
                content: "old content".to_string(),
                checksum: MemoryChecksum::from_content("old content"),
                created_at: older,
                updated_at: older,
                tombstoned: false,
            },
        ];

        let count = svc.apply_remote(remote_entries).unwrap();
        assert_eq!(count, 0);

        let local = svc.read("local").unwrap().unwrap();
        assert_eq!(local.content, "newer content");
    }

    #[test]
    fn upsert_un_tombstones() {
        let (svc, _tmp) = temp_service();
        svc.upsert("x", "val").unwrap();
        svc.delete("x").unwrap();
        svc.upsert("x", "new val").unwrap();

        let entry = svc.read("x").unwrap().unwrap();
        assert_eq!(entry.content, "new val");
        assert!(!entry.tombstoned);
    }

    #[test]
    fn org_scoped_service_uses_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = TeamMemorySyncService::new_with_org(
            "repo1".to_string(),
            tmp.path().to_path_buf(),
            "acme-corp".to_string(),
        );
        svc.upsert("test", "content").unwrap();

        // Should be stored under org subdirectory
        let org_dir = tmp.path().join("acme-corp");
        assert!(org_dir.exists());
        assert!(org_dir.join("test.json").exists());
    }

    #[test]
    fn org_scoped_isolation() {
        let tmp = tempfile::tempdir().unwrap();

        let svc1 = TeamMemorySyncService::new_with_org(
            "repo1".to_string(),
            tmp.path().to_path_buf(),
            "acme".to_string(),
        );
        svc1.upsert("shared-topic", "acme-content").unwrap();

        let svc2 = TeamMemorySyncService::new_with_org(
            "repo1".to_string(),
            tmp.path().to_path_buf(),
            "other".to_string(),
        );
        svc2.upsert("shared-topic", "other-content").unwrap();

        // Each org has its own copy
        let acme = svc1.read("shared-topic").unwrap().unwrap();
        let other = svc2.read("shared-topic").unwrap().unwrap();
        assert_eq!(acme.content, "acme-content");
        assert_eq!(other.content, "other-content");
    }

    #[test]
    fn org_scoped_service_builder() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = TeamMemorySyncService::new("repo1".to_string(), tmp.path().to_path_buf())
            .with_org("acme".to_string());

        assert_eq!(svc.org_id.as_deref(), Some("acme"));
    }

    #[test]
    fn org_none_uses_base_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = TeamMemorySyncService::new("repo1".to_string(), tmp.path().to_path_buf());
        svc.upsert("test", "content").unwrap();

        // Should be stored directly in memory_dir, no subdirectory
        assert!(tmp.path().join("test.json").exists());
    }

    #[test]
    fn org_scoped_sync_delta_isolated() {
        let tmp = tempfile::tempdir().unwrap();

        let svc1 = TeamMemorySyncService::new_with_org(
            "repo1".to_string(),
            tmp.path().to_path_buf(),
            "org-a".to_string(),
        );
        let svc2 = TeamMemorySyncService::new_with_org(
            "repo1".to_string(),
            tmp.path().to_path_buf(),
            "org-b".to_string(),
        );

        svc1.upsert("a-only", "content-a").unwrap();
        svc2.upsert("b-only", "content-b").unwrap();

        // Each service's list only sees its own org's entries
        let list1 = svc1.list().unwrap();
        let list2 = svc2.list().unwrap();

        assert_eq!(list1.len(), 1);
        assert_eq!(list1[0].topic, "a-only");
        assert_eq!(list2.len(), 1);
        assert_eq!(list2[0].topic, "b-only");
    }
}
