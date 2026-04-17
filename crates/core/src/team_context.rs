//! Team context and cross-worktree session recovery.
//!
//! Phase 11.3: Shared context views for team members and recent session
//! pointer for cross-worktree resume.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ─── TeamContext ────────────────────────────────────────────────────────────

/// Shared context visible to all team members.
///
/// This is the "live" view of team state beyond the sync snapshot —
/// includes recent activity, in-progress work, and shared notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamContext {
    /// The team this context belongs to.
    pub team_id: String,
    /// Shared notes visible to all members.
    pub shared_notes: Vec<Note>,
    /// Recent activity log (newest first, bounded).
    pub recent_activity: Vec<ActivityEntry>,
    /// In-progress work items (not yet in shared task view).
    pub in_progress: Vec<WorkItem>,
    /// Maximum number of activity entries to retain.
    #[serde(default = "default_activity_limit")]
    pub activity_limit: usize,
}

const fn default_activity_limit() -> usize {
    100
}

impl TeamContext {
    /// Create a new empty team context.
    #[must_use]
    pub fn new(team_id: String) -> Self {
        Self {
            team_id,
            shared_notes: Vec::new(),
            recent_activity: Vec::new(),
            in_progress: Vec::new(),
            activity_limit: default_activity_limit(),
        }
    }

    /// Add a shared note visible to all team members.
    pub fn add_note(&mut self, title: String, content: String, author_id: String) {
        self.shared_notes.push(Note {
            id: format!("note_{}", self.shared_notes.len() + 1),
            title,
            content,
            author_id,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
    }

    /// Record an activity entry.
    ///
    /// If the activity log exceeds the limit, the oldest entries are dropped.
    pub fn record_activity(&mut self, member_id: String, description: String) {
        self.recent_activity.insert(0, ActivityEntry {
            id: format!("act_{}", self.recent_activity.len() + 1),
            member_id,
            description,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });

        // Trim to limit
        if self.recent_activity.len() > self.activity_limit {
            self.recent_activity.truncate(self.activity_limit);
        }
    }

    /// Add an in-progress work item.
    pub fn add_work_item(&mut self, item: WorkItem) {
        self.in_progress.push(item);
    }

    /// Complete an in-progress work item by ID.
    pub fn complete_work_item(&mut self, item_id: &str) {
        self.in_progress.retain(|w| w.id != item_id);
    }

    /// Get the latest N activity entries.
    #[must_use]
    pub fn recent_activity(&self, limit: usize) -> &[ActivityEntry] {
        let n = limit.min(self.recent_activity.len());
        &self.recent_activity[..n]
    }
}

/// A note shared among team members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    /// Unique note ID.
    pub id: String,
    /// Note title.
    pub title: String,
    /// Note content.
    pub content: String,
    /// Member who created this note.
    pub author_id: String,
    /// Unix timestamp when created.
    pub created_at: u64,
}

/// An activity entry in the team log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Unique activity ID.
    pub id: String,
    /// Member who performed the activity.
    pub member_id: String,
    /// Description of what happened.
    pub description: String,
    /// Unix timestamp when recorded.
    pub timestamp: u64,
}

/// An in-progress work item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    /// Unique work item ID.
    pub id: String,
    /// Description of the work.
    pub description: String,
    /// Member working on this item.
    pub worker_id: String,
    /// Estimated remaining steps.
    pub remaining_steps: usize,
}

// ─── SessionPointer (Cross-Worktree Recovery) ──────────────────────────────

/// A pointer to a recent session for cross-worktree recovery.
///
/// When a user resumes a session from a different worktree, this
/// pointer helps locate the most recent session state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPointer {
    /// Session ID.
    pub session_id: String,
    /// Project hash (repo identifier).
    pub project_hash: String,
    /// Working directory of the session.
    pub cwd: PathBuf,
    /// Branch name the session was on.
    pub branch: String,
    /// Worktree path if applicable.
    pub worktree_path: Option<PathBuf>,
    /// Last active timestamp.
    pub last_active_at: u64,
    /// Message count at last activity.
    pub message_count: usize,
}

impl SessionPointer {
    /// Whether this session was in a worktree.
    #[must_use]
    pub fn is_worktree(&self) -> bool {
        self.worktree_path.is_some()
    }
}

/// Stores and retrieves recent session pointers for cross-worktree recovery.
#[derive(Debug)]
pub struct SessionPointerStore {
    /// Recent sessions per project (project_hash -> list of pointers).
    sessions: HashMap<String, Vec<SessionPointer>>,
    /// Maximum number of session pointers per project.
    max_per_project: usize,
}

impl Default for SessionPointerStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionPointerStore {
    /// Create a new store with default limits.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            max_per_project: 10,
        }
    }

    /// Set the maximum pointers per project.
    pub const fn with_max_per_project(mut self, n: usize) -> Self {
        self.max_per_project = n;
        self
    }

    /// Register a new session pointer.
    ///
    /// Updates the last-active time if the session already exists.
    pub fn register(&mut self, pointer: SessionPointer) {
        let project_hash = pointer.project_hash.clone();
        let entries = self.sessions.entry(project_hash).or_default();

        // Update existing if same session_id
        if let Some(existing) = entries.iter_mut().find(|e| e.session_id == pointer.session_id) {
            existing.last_active_at = pointer.last_active_at;
            existing.message_count = pointer.message_count;
            existing.cwd = pointer.cwd;
            existing.branch = pointer.branch.clone();
            existing.worktree_path = pointer.worktree_path.clone();
        } else {
            entries.push(pointer);
            // Sort by last_active_at descending (most recent first)
            entries.sort_by_key(|e| std::cmp::Reverse(e.last_active_at));
            // Trim to max
            if entries.len() > self.max_per_project {
                entries.truncate(self.max_per_project);
            }
        }
    }

    /// Get the most recent session for a project.
    ///
    /// Optionally filters to sessions on a specific worktree path.
    #[must_use]
    pub fn most_recent(
        &self,
        project_hash: &str,
        worktree_filter: Option<&PathBuf>,
    ) -> Option<&SessionPointer> {
        self.sessions
            .get(project_hash)
            .into_iter()
            .flat_map(|v| v.iter())
            .find(|s| {
                worktree_filter.map_or(true, |wp| s.worktree_path.as_ref() == Some(wp))
            })
    }

    /// Get all recent sessions for a project.
    #[must_use]
    pub fn recent_sessions(&self, project_hash: &str) -> Vec<&SessionPointer> {
        self.sessions
            .get(project_hash)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// Update the last-active timestamp for a session.
    pub fn touch(&mut self, project_hash: &str, session_id: &str, timestamp: u64) {
        if let Some(entries) = self.sessions.get_mut(project_hash) {
            if let Some(entry) = entries.iter_mut().find(|e| e.session_id == session_id) {
                entry.last_active_at = timestamp;
                // Re-sort
                entries.sort_by_key(|e| std::cmp::Reverse(e.last_active_at));
            }
        }
    }

    /// Remove sessions for a project (cleanup on project leave).
    pub fn clear_project(&mut self, project_hash: &str) {
        self.sessions.remove(project_hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── TeamContext tests ─────────────────────────────────────────────────

    #[test]
    fn team_context_starts_empty() {
        let ctx = TeamContext::new("team_1".into());
        assert!(ctx.shared_notes.is_empty());
        assert!(ctx.recent_activity.is_empty());
        assert!(ctx.in_progress.is_empty());
    }

    #[test]
    fn team_context_add_note() {
        let mut ctx = TeamContext::new("team_1".into());
        ctx.add_note("Plan".into(), "Do X, then Y".into(), "m1".into());
        assert_eq!(ctx.shared_notes.len(), 1);
        assert_eq!(ctx.shared_notes[0].title, "Plan");
        assert_eq!(ctx.shared_notes[0].author_id, "m1");
    }

    #[test]
    fn team_context_records_activity() {
        let mut ctx = TeamContext::new("team_1".into());
        ctx.record_activity("m1".into(), "completed task A".into());
        ctx.record_activity("m2".into(), "started task B".into());

        let recent = ctx.recent_activity(10);
        assert_eq!(recent.len(), 2);
        // Newest first
        assert_eq!(recent[0].description, "started task B");
        assert_eq!(recent[1].description, "completed task A");
    }

    #[test]
    fn team_context_activity_trimming() {
        let mut ctx = TeamContext::new("team_1".into());
        ctx.activity_limit = 3;

        for i in 0..5 {
            ctx.record_activity("m1".into(), format!("activity {i}"));
        }

        assert_eq!(ctx.recent_activity.len(), 3);
        // Most recent 3: activities 4, 3, 2 (newest first)
        assert_eq!(ctx.recent_activity[0].description, "activity 4");
    }

    #[test]
    fn team_context_work_item_lifecycle() {
        let mut ctx = TeamContext::new("team_1".into());
        ctx.add_work_item(WorkItem {
            id: "w1".into(),
            description: "Fix bug".into(),
            worker_id: "m1".into(),
            remaining_steps: 3,
        });
        assert_eq!(ctx.in_progress.len(), 1);

        ctx.complete_work_item("w1");
        assert!(ctx.in_progress.is_empty());
    }

    // ─── SessionPointerStore tests ─────────────────────────────────────────

    #[test]
    fn store_register_and_retrieve() {
        let mut store = SessionPointerStore::new();
        store.register(SessionPointer {
            session_id: "sess_1".into(),
            project_hash: "proj_abc".into(),
            cwd: PathBuf::from("/src"),
            branch: "main".into(),
            worktree_path: None,
            last_active_at: 1000,
            message_count: 10,
        });

        let recent = store.most_recent("proj_abc", None);
        assert!(recent.is_some());
        assert_eq!(recent.unwrap().session_id, "sess_1");
    }

    #[test]
    fn store_most_recent_is_latest() {
        let mut store = SessionPointerStore::new();
        store.register(SessionPointer {
            session_id: "sess_old".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "main".into(),
            worktree_path: None,
            last_active_at: 500,
            message_count: 5,
        });
        store.register(SessionPointer {
            session_id: "sess_new".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "feat".into(),
            worktree_path: None,
            last_active_at: 1000,
            message_count: 20,
        });

        let recent = store.most_recent("p1", None);
        assert_eq!(recent.unwrap().session_id, "sess_new");
    }

    #[test]
    fn store_worktree_filter() {
        let mut store = SessionPointerStore::new();
        let wt_path = PathBuf::from("/worktrees/coderex-1");
        store.register(SessionPointer {
            session_id: "sess_wt".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "feat".into(),
            worktree_path: Some(wt_path.clone()),
            last_active_at: 1000,
            message_count: 10,
        });
        store.register(SessionPointer {
            session_id: "sess_main".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "main".into(),
            worktree_path: None,
            last_active_at: 900,
            message_count: 5,
        });

        // Filter to worktree sessions only
        let recent = store.most_recent("p1", Some(&wt_path));
        assert_eq!(recent.unwrap().session_id, "sess_wt");
    }

    #[test]
    fn store_recent_sessions() {
        let mut store = SessionPointerStore::new();
        for i in 0..5 {
            store.register(SessionPointer {
                session_id: format!("sess_{i}"),
                project_hash: "p1".into(),
                cwd: PathBuf::from("/src"),
                branch: "main".into(),
                worktree_path: None,
                last_active_at: 1000 - i as u64,
                message_count: i,
            });
        }

        let sessions = store.recent_sessions("p1");
        assert_eq!(sessions.len(), 5);
        // Sorted by last_active_at descending
        assert_eq!(sessions[0].session_id, "sess_0");
    }

    #[test]
    fn store_trims_to_max() {
        let mut store = SessionPointerStore::new().with_max_per_project(3);
        for i in 0..5 {
            store.register(SessionPointer {
                session_id: format!("sess_{i}"),
                project_hash: "p1".into(),
                cwd: PathBuf::from("/src"),
                branch: "main".into(),
                worktree_path: None,
                last_active_at: i as u64,
                message_count: i,
            });
        }

        let sessions = store.recent_sessions("p1");
        assert_eq!(sessions.len(), 3);
        // Most recent (highest timestamp) should be first
        assert_eq!(sessions[0].session_id, "sess_4");
    }

    #[test]
    fn store_update_existing() {
        let mut store = SessionPointerStore::new();
        store.register(SessionPointer {
            session_id: "sess_1".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "main".into(),
            worktree_path: None,
            last_active_at: 100,
            message_count: 5,
        });
        // Update with new timestamp
        store.register(SessionPointer {
            session_id: "sess_1".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "main".into(),
            worktree_path: None,
            last_active_at: 200,
            message_count: 15,
        });

        let sessions = store.recent_sessions("p1");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].message_count, 15);
        assert_eq!(sessions[0].last_active_at, 200);
    }

    #[test]
    fn store_touch() {
        let mut store = SessionPointerStore::new();
        store.register(SessionPointer {
            session_id: "sess_1".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "main".into(),
            worktree_path: None,
            last_active_at: 100,
            message_count: 5,
        });

        store.touch("p1", "sess_1", 999);
        let session = store.most_recent("p1", None).unwrap();
        assert_eq!(session.last_active_at, 999);
    }

    #[test]
    fn store_clear_project() {
        let mut store = SessionPointerStore::new();
        store.register(SessionPointer {
            session_id: "sess_1".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "main".into(),
            worktree_path: None,
            last_active_at: 100,
            message_count: 5,
        });

        store.clear_project("p1");
        assert!(store.recent_sessions("p1").is_empty());
    }

    #[test]
    fn session_pointer_is_worktree() {
        let wt = SessionPointer {
            session_id: "s1".into(),
            project_hash: "p1".into(),
            cwd: PathBuf::from("/src"),
            branch: "feat".into(),
            worktree_path: Some(PathBuf::from("/wt")),
            last_active_at: 100,
            message_count: 5,
        };
        let non_wt = SessionPointer {
            worktree_path: None,
            ..wt.clone()
        };

        assert!(wt.is_worktree());
        assert!(!non_wt.is_worktree());
    }

    #[test]
    fn note_serialization() {
        let note = Note {
            id: "n1".into(),
            title: "My Note".into(),
            content: "Content here".into(),
            author_id: "m1".into(),
            created_at: 12345,
        };
        let json = serde_json::to_string(&note).unwrap();
        assert!(json.contains("My Note"));
        assert!(json.contains("m1"));
    }
}
