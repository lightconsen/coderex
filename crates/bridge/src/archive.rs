//! Bridge archive and cleanup — transcript archiving and worktree cleanup.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Archived session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionArchive {
    /// Session ID.
    pub session_id: String,
    /// Path to the archived transcript.
    pub transcript_path: PathBuf,
    /// Usage statistics.
    pub usage: UsageStats,
    /// Completion status.
    pub status: String,
    /// Timestamp of archival.
    pub archived_at: String,
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    /// Input tokens.
    pub input_tokens: usize,
    /// Output tokens.
    pub output_tokens: usize,
    /// Cache read tokens.
    pub cache_read_tokens: usize,
    /// Cache creation tokens.
    pub cache_creation_tokens: usize,
    /// Cost in USD.
    pub cost_usd: f64,
}

/// Writes session archives.
#[derive(Debug)]
pub struct BridgeArchiveWriter {
    /// Base directory for archives.
    pub archive_dir: PathBuf,
}

impl BridgeArchiveWriter {
    /// Create a new archive writer.
    #[must_use]
    pub fn new(archive_dir: PathBuf) -> Self {
        Self { archive_dir }
    }

    /// Archive a session's transcript and metadata.
    #[allow(clippy::unused_async)]
    pub async fn archive(
        &self,
        session_id: &str,
        transcript: &str,
        usage: UsageStats,
        status: &str,
    ) -> Result<SessionArchive, String> {
        let session_dir = self.archive_dir.join(session_id);
        std::fs::create_dir_all(&session_dir)
            .map_err(|e| format!("failed to create archive dir: {e}"))?;

        let transcript_path = session_dir.join("transcript.jsonl");
        std::fs::write(&transcript_path, transcript)
            .map_err(|e| format!("failed to write transcript: {e}"))?;

        let archive = SessionArchive {
            session_id: session_id.to_string(),
            transcript_path: transcript_path.clone(),
            usage,
            status: status.to_string(),
            archived_at: chrono::Utc::now().to_rfc3339(),
        };

        let metadata_path = session_dir.join("metadata.json");
        let metadata = serde_json::to_string_pretty(&archive)
            .map_err(|e| format!("failed to serialize metadata: {e}"))?;
        std::fs::write(&metadata_path, metadata)
            .map_err(|e| format!("failed to write metadata: {e}"))?;

        Ok(archive)
    }

    /// List archived sessions.
    #[must_use]
    pub fn list_archives(&self) -> Vec<String> {
        if !self.archive_dir.exists() {
            return Vec::new();
        }
        std::fs::read_dir(&self.archive_dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Load archived metadata for a session.
    #[must_use]
    pub fn load_metadata(&self, session_id: &str) -> Option<SessionArchive> {
        let metadata_path = self.archive_dir.join(session_id).join("metadata.json");
        if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path).ok()?;
            serde_json::from_str(&content).ok()
        } else {
            None
        }
    }
}

/// Manages cleanup of bridge session resources including process termination.
#[derive(Debug)]
pub struct BridgeCleanupManager {
    /// Base directory for worktrees.
    pub worktree_base: PathBuf,
    /// Base directory for session data.
    pub session_base: PathBuf,
    /// Whether to confirm stopWork after cleanup.
    pub confirm_stop_work: bool,
    /// Tracked process IDs for active sessions.
    process_map: HashMap<String, u32>,
}

impl BridgeCleanupManager {
    /// Create a new cleanup manager.
    #[must_use]
    pub fn new(worktree_base: PathBuf, session_base: PathBuf) -> Self {
        Self {
            worktree_base,
            session_base,
            confirm_stop_work: true,
            process_map: HashMap::new(),
        }
    }

    /// Register a session's process for tracking.
    pub fn track_process(&mut self, session_id: &str, pid: u32) {
        self.process_map.insert(session_id.to_string(), pid);
    }

    /// Unregister a session's process.
    pub fn untrack_process(&mut self, session_id: &str) {
        self.process_map.remove(session_id);
    }

    /// Clean up all resources for a session.
    pub fn cleanup(&mut self, session_id: &str) -> Result<CleanupReport, String> {
        let mut report = CleanupReport {
            session_id: session_id.to_string(),
            worktree_removed: false,
            session_dir_removed: false,
            process_terminated: false,
        };

        // Terminate session process if tracked
        if let Some(pid) = self.process_map.remove(session_id) {
            report.process_terminated = self.terminate_process(pid);
        }

        // Remove worktree
        let worktree_path = self.worktree_base.join(session_id);
        if worktree_path.exists() {
            std::fs::remove_dir_all(&worktree_path)
                .map_err(|e| format!("failed to remove worktree: {e}"))?;
            report.worktree_removed = true;
        }

        // Remove session directory
        let session_path = self.session_base.join(session_id);
        if session_path.exists() {
            std::fs::remove_dir_all(&session_path)
                .map_err(|e| format!("failed to remove session dir: {e}"))?;
            report.session_dir_removed = true;
        }

        Ok(report)
    }

    /// Terminate a process by PID.
    fn terminate_process(&self, pid: u32) -> bool {
        // Use platform-appropriate process termination
        #[cfg(unix)]
        {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .output();
            true
        }

        #[cfg(not(unix))]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .output();
            true
        }
    }

    /// Get the number of tracked processes.
    #[must_use]
    pub fn tracked_process_count(&self) -> usize {
        self.process_map.len()
    }

    /// Check if a session has a tracked process.
    #[must_use]
    pub fn has_tracked_process(&self, session_id: &str) -> bool {
        self.process_map.contains_key(session_id)
    }
}

/// Report of cleanup operations.
#[derive(Debug)]
pub struct CleanupReport {
    /// Session ID.
    pub session_id: String,
    /// Whether the worktree was removed.
    pub worktree_removed: bool,
    /// Whether the session directory was removed.
    pub session_dir_removed: bool,
    /// Whether the process was terminated.
    pub process_terminated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_usage() -> UsageStats {
        UsageStats {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_creation_tokens: 100,
            cost_usd: 0.05,
        }
    }

    #[test]
    fn archive_session() {
        let dir = tempfile::tempdir().unwrap();
        let writer = BridgeArchiveWriter::new(dir.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let archive = rt
            .block_on(writer.archive("s1", "line1\nline2\n", sample_usage(), "completed"))
            .unwrap();
        assert_eq!(archive.session_id, "s1");
        assert_eq!(archive.status, "completed");
        assert!(archive.transcript_path.exists());
    }

    #[test]
    fn list_archives() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("s1")).unwrap();
        std::fs::create_dir_all(dir.path().join("s2")).unwrap();
        let writer = BridgeArchiveWriter::new(dir.path().to_path_buf());
        let archives = writer.list_archives();
        assert_eq!(archives.len(), 2);
    }

    #[test]
    fn list_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let writer = BridgeArchiveWriter::new(dir.path().to_path_buf());
        assert!(writer.list_archives().is_empty());
    }

    #[test]
    fn cleanup_removes_dirs() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("worktrees/s1")).unwrap();
        std::fs::create_dir_all(base.path().join("sessions/s1")).unwrap();
        let mut mgr = BridgeCleanupManager::new(
            base.path().join("worktrees"),
            base.path().join("sessions"),
        );
        let report = mgr.cleanup("s1").unwrap();
        assert!(report.worktree_removed);
        assert!(report.session_dir_removed);
    }

    #[test]
    fn cleanup_nonexistent_is_ok() {
        let base = tempfile::tempdir().unwrap();
        let mut mgr = BridgeCleanupManager::new(
            base.path().join("worktrees"),
            base.path().join("sessions"),
        );
        let report = mgr.cleanup("nonexistent").unwrap();
        assert!(!report.worktree_removed);
        assert!(!report.session_dir_removed);
    }

    #[test]
    fn process_tracking() {
        let mut mgr = BridgeCleanupManager::new(
            PathBuf::from("/tmp/worktrees"),
            PathBuf::from("/tmp/sessions"),
        );
        assert_eq!(mgr.tracked_process_count(), 0);

        mgr.track_process("s1", 12345);
        assert_eq!(mgr.tracked_process_count(), 1);
        assert!(mgr.has_tracked_process("s1"));
        assert!(!mgr.has_tracked_process("s2"));

        mgr.untrack_process("s1");
        assert_eq!(mgr.tracked_process_count(), 0);
    }

    #[test]
    fn load_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let writer = BridgeArchiveWriter::new(dir.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(writer.archive("s1", "line\n", sample_usage(), "completed")).unwrap();

        let metadata = writer.load_metadata("s1");
        assert!(metadata.is_some());
        let meta = metadata.unwrap();
        assert_eq!(meta.session_id, "s1");
        assert_eq!(meta.status, "completed");

        assert!(writer.load_metadata("nonexistent").is_none());
    }
}
