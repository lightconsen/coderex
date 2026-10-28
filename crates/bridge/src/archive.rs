//! Bridge archive and cleanup — transcript archiving and worktree cleanup.

use coderex_core::process_tree::ProcessTree;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Current archive format version.
///
/// Increment this when the archive schema changes.
/// Migrations should be added for each version bump.
pub const CURRENT_ARCHIVE_VERSION: u32 = 1;

/// Archived session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionArchive {
    /// Archive format version.
    pub version: u32,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    pub const fn new(archive_dir: PathBuf) -> Self {
        Self { archive_dir }
    }

    /// Archive a session's transcript and metadata.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
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
            version: CURRENT_ARCHIVE_VERSION,
            session_id: session_id.to_string(),
            transcript_path,
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
                    .filter_map(std::result::Result::ok)
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Load archived metadata for a session.
    ///
    /// Returns `None` if the archive does not exist or uses an unsupported version.
    #[must_use]
    pub fn load_metadata(&self, session_id: &str) -> Option<SessionArchive> {
        let metadata_path = self.archive_dir.join(session_id).join("metadata.json");
        if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path).ok()?;
            let archive: SessionArchive = serde_json::from_str(&content).ok()?;
            if archive.version > CURRENT_ARCHIVE_VERSION {
                // Future version — cannot parse safely
                return None;
            }
            Some(archive)
        } else {
            None
        }
    }
}

/// What cleanup knows about a session's process.
///
/// A [`ProcessTree`] can kill the whole tree. A bare pid is the older,
/// weaker shape: on Unix it signals one process, and on Windows the pid is
/// placed in a job before terminating so descendants go with it.
#[derive(Debug)]
enum TrackedProcess {
    Tree(Arc<ProcessTree>),
    Pid(u32),
}

/// The shared registry of a session's live process.
///
/// The cleanup manager owns one and hands out clones, so the place that
/// actually spawns a session's child — the CLI's session runner — registers
/// the child in the same map that [`BridgeCleanupManager::cleanup`] reads.
/// Without that, the map is always empty for a real session and cleanup
/// terminates nothing.
#[derive(Debug, Clone, Default)]
pub struct ProcessRegistry {
    entries: Arc<Mutex<HashMap<String, TrackedProcess>>>,
}

impl ProcessRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a session's process tree.
    pub fn track_tree(&self, session_id: &str, tree: Arc<ProcessTree>) {
        lock_entries(&self.entries).insert(session_id.to_string(), TrackedProcess::Tree(tree));
    }

    /// Register a session's bare pid.
    ///
    /// Prefer [`Self::track_tree`] where the spawn site has one: a pid alone
    /// cannot reach descendants on Unix and only reaches them best-effort on
    /// Windows.
    pub fn track_process(&self, session_id: &str, pid: u32) {
        lock_entries(&self.entries).insert(session_id.to_string(), TrackedProcess::Pid(pid));
    }

    /// Remove a session's process. Returns whether one was registered.
    pub fn untrack(&self, session_id: &str) -> bool {
        lock_entries(&self.entries).remove(session_id).is_some()
    }

    /// The number of processes currently registered.
    #[must_use]
    pub fn len(&self) -> usize {
        lock_entries(&self.entries).len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        lock_entries(&self.entries).is_empty()
    }

    /// Whether a session's process is registered.
    #[must_use]
    pub fn contains(&self, session_id: &str) -> bool {
        lock_entries(&self.entries).contains_key(session_id)
    }

    /// Remove and return a session's tracked process, for the manager to
    /// terminate.
    fn take(&self, session_id: &str) -> Option<TrackedProcess> {
        lock_entries(&self.entries).remove(session_id)
    }
}

/// Lock the registry map, recovering from a poisoned lock.
///
/// A panic while holding the lock cannot leave the map in a half-written
/// state that matters here, so a poisoned lock is not a reason to fail
/// cleanup.
fn lock_entries(
    entries: &Mutex<HashMap<String, TrackedProcess>>,
) -> std::sync::MutexGuard<'_, HashMap<String, TrackedProcess>> {
    entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    /// Tracked processes for active sessions.
    processes: ProcessRegistry,
}

impl BridgeCleanupManager {
    /// Create a new cleanup manager.
    #[must_use]
    pub fn new(worktree_base: PathBuf, session_base: PathBuf) -> Self {
        Self {
            worktree_base,
            session_base,
            confirm_stop_work: true,
            processes: ProcessRegistry::new(),
        }
    }

    /// The shared registry a spawn site registers a session's child in.
    #[must_use]
    pub fn process_registry(&self) -> ProcessRegistry {
        self.processes.clone()
    }

    /// Register a session's process for tracking.
    ///
    /// Prefer [`Self::track_tree`] where the spawn site has one: a pid alone
    /// cannot reach descendants on Unix and only reaches them best-effort on
    /// Windows.
    pub fn track_process(&mut self, session_id: &str, pid: u32) {
        self.processes.track_process(session_id, pid);
    }

    /// Register a session's process tree for tracking.
    pub fn track_tree(&mut self, session_id: &str, tree: Arc<ProcessTree>) {
        self.processes.track_tree(session_id, tree);
    }

    /// Unregister a session's process.
    pub fn untrack_process(&mut self, session_id: &str) {
        self.processes.untrack(session_id);
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
        if let Some(tracked) = self.processes.take(session_id) {
            report.process_terminated = Self::terminate_process(&tracked);
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

    /// Terminate a tracked process.
    ///
    /// Returns whether the kill was actually issued. It used to always return
    /// `true`, so `CleanupReport::process_terminated` reported success even
    /// when `kill`/`taskkill` failed.
    fn terminate_process(tracked: &TrackedProcess) -> bool {
        match tracked {
            TrackedProcess::Tree(tree) => tree.terminate(),
            TrackedProcess::Pid(pid) => {
                let Ok(tree) = ProcessTree::from_pid(*pid) else {
                    return false;
                };
                if tree.attach_pid(*pid).is_err() {
                    return false;
                }
                tree.terminate()
            }
        }
    }

    /// Get the number of tracked processes.
    #[must_use]
    pub fn tracked_process_count(&self) -> usize {
        self.processes.len()
    }

    /// Check if a session has a tracked process.
    #[must_use]
    pub fn has_tracked_process(&self, session_id: &str) -> bool {
        self.processes.contains(session_id)
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
        let mut mgr =
            BridgeCleanupManager::new(base.path().join("worktrees"), base.path().join("sessions"));
        let report = mgr.cleanup("s1").unwrap();
        assert!(report.worktree_removed);
        assert!(report.session_dir_removed);
    }

    #[test]
    fn cleanup_nonexistent_is_ok() {
        let base = tempfile::tempdir().unwrap();
        let mut mgr =
            BridgeCleanupManager::new(base.path().join("worktrees"), base.path().join("sessions"));
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

    /// Cleanup kills a tracked tree, not just its direct child.
    ///
    /// The Windows arm is the one that matters: it used to run
    /// `taskkill /F /PID` without `/T`, leaving descendants behind.
    #[test]
    fn cleanup_terminates_a_tracked_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = BridgeCleanupManager::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        let tree = Arc::new(
            ProcessTree::create(coderex_core::process_tree::JobLimits::default()).unwrap(),
        );

        #[cfg(unix)]
        let mut cmd = {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "sleep 60"]);
            cmd
        };
        #[cfg(target_os = "windows")]
        let mut cmd = {
            let mut cmd = std::process::Command::new("cmd.exe");
            cmd.args(["/C", "ping -n 60 127.0.0.1 > nul"]);
            cmd
        };
        #[cfg(not(any(unix, target_os = "windows")))]
        let mut cmd = std::process::Command::new("true");

        tree.prepare(&mut cmd);
        let mut child = cmd.spawn().expect("the test child should spawn");
        tree.attach(&child).expect("the child should join its tree");
        mgr.track_tree("s1", Arc::clone(&tree));

        let report = mgr.cleanup("s1").unwrap();
        assert!(report.process_terminated, "the kill should be issued");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the tracked process survived cleanup"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn load_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let writer = BridgeArchiveWriter::new(dir.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(writer.archive("s1", "line\n", sample_usage(), "completed"))
            .unwrap();

        let metadata = writer.load_metadata("s1");
        assert!(metadata.is_some());
        let meta = metadata.unwrap();
        assert_eq!(meta.session_id, "s1");
        assert_eq!(meta.status, "completed");

        assert!(writer.load_metadata("nonexistent").is_none());
    }

    #[test]
    fn archive_version_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let writer = BridgeArchiveWriter::new(dir.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let archive = rt
            .block_on(writer.archive("s1", "line\n", sample_usage(), "completed"))
            .unwrap();
        assert_eq!(archive.version, CURRENT_ARCHIVE_VERSION);
    }

    #[test]
    fn load_metadata_rejects_future_version() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("s1");
        std::fs::create_dir_all(&session_dir).unwrap();
        // Write a future-version archive manually
        let future_meta = serde_json::json!({
            "version": 999,
            "session_id": "s1",
            "transcript_path": session_dir.join("transcript.jsonl"),
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_tokens": 0,
                "cache_creation_tokens": 0,
                "cost_usd": 0.0
            },
            "status": "completed",
            "archived_at": "2026-01-01T00:00:00Z"
        });
        std::fs::write(
            session_dir.join("metadata.json"),
            serde_json::to_string(&future_meta).unwrap(),
        )
        .unwrap();

        let writer = BridgeArchiveWriter::new(dir.path().to_path_buf());
        assert!(writer.load_metadata("s1").is_none());
    }
}
