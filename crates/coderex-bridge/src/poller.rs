//! Bridge work poller — environment registration and capacity reporting.

use serde::{Deserialize, Serialize};

/// Work assignment from the control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkAssignment {
    /// Unique assignment ID.
    pub assignment_id: String,
    /// Session ID to spawn.
    pub session_id: String,
    /// Working directory for the session.
    pub working_directory: String,
    /// Initial prompt for the session.
    pub prompt: String,
    /// Spawn mode: single-session, same-dir, or worktree.
    pub spawn_mode: SpawnMode,
}

/// How a session should be spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnMode {
    /// Single session in the given directory.
    SingleSession,
    /// Multiple sessions sharing the same directory.
    SameDir,
    /// Create a git worktree for this session.
    Worktree,
}

/// Capacity report from this environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityReport {
    /// Number of currently active sessions.
    pub active_sessions: usize,
    /// Maximum concurrent sessions allowed.
    pub max_concurrent: usize,
    /// Available slots.
    pub available_slots: usize,
    /// Environment identifier.
    pub env_id: String,
    /// Git repository root (if in a git repo).
    pub git_root: Option<String>,
}

/// Polls or receives work assignments from the remote control plane.
#[derive(Debug)]
pub struct BridgeWorkPoller {
    /// This environment's identifier.
    pub env_id: String,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Pending work assignments.
    pending: Vec<WorkAssignment>,
    /// Active session count.
    active_sessions: usize,
}

impl BridgeWorkPoller {
    /// Create a new work poller.
    #[must_use]
    pub fn new(env_id: String, max_concurrent: usize) -> Self {
        Self {
            env_id,
            max_concurrent,
            pending: Vec::new(),
            active_sessions: 0,
        }
    }

    /// Register this environment with the control plane.
    #[allow(clippy::unused_async)]
    pub async fn register(&self) -> Result<(), String> {
        // TODO: HTTP registration with control plane
        Ok(())
    }

    /// Report current capacity.
    #[must_use]
    pub fn report_capacity(&self) -> CapacityReport {
        CapacityReport {
            active_sessions: self.active_sessions,
            max_concurrent: self.max_concurrent,
            available_slots: self.max_concurrent.saturating_sub(self.active_sessions),
            env_id: self.env_id.clone(),
            git_root: None,
        }
    }

    /// Receive a work assignment.
    pub fn receive_assignment(&mut self, assignment: WorkAssignment) {
        self.pending.push(assignment);
    }

    /// Take the next pending assignment.
    pub fn take_assignment(&mut self) -> Option<WorkAssignment> {
        if self.active_sessions < self.max_concurrent {
            if !self.pending.is_empty() {
                self.active_sessions += 1;
                return Some(self.pending.remove(0));
            }
        }
        None
    }

    /// Mark a session as completed, freeing capacity.
    pub fn session_completed(&mut self) {
        if self.active_sessions > 0 {
            self.active_sessions -= 1;
        }
    }

    /// Check if there are pending assignments.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Number of pending assignments.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_assignment() -> WorkAssignment {
        WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp/work".into(),
            prompt: "Fix the bug".into(),
            spawn_mode: SpawnMode::SingleSession,
        }
    }

    #[test]
    fn capacity_report() {
        let poller = BridgeWorkPoller::new("env1".into(), 5);
        let report = poller.report_capacity();
        assert_eq!(report.env_id, "env1");
        assert_eq!(report.max_concurrent, 5);
        assert_eq!(report.available_slots, 5);
    }

    #[test]
    fn receive_and_take_assignment() {
        let mut poller = BridgeWorkPoller::new("env1".into(), 5);
        poller.receive_assignment(sample_assignment());
        assert!(poller.has_pending());
        assert_eq!(poller.pending_count(), 1);

        let assignment = poller.take_assignment().unwrap();
        assert_eq!(assignment.assignment_id, "a1");
        assert_eq!(poller.pending_count(), 0);
    }

    #[test]
    fn capacity_limits_take() {
        let mut poller = BridgeWorkPoller::new("env1".into(), 1);
        poller.receive_assignment(sample_assignment());
        poller.receive_assignment(WorkAssignment {
            assignment_id: "a2".into(),
            session_id: "s2".into(),
            working_directory: "/tmp".into(),
            prompt: "Second".into(),
            spawn_mode: SpawnMode::SameDir,
        });

        // First take succeeds
        assert!(poller.take_assignment().is_some());
        // Second take fails (at capacity)
        assert!(poller.take_assignment().is_none());

        // Complete a session, freeing capacity
        poller.session_completed();
        assert!(poller.take_assignment().is_some());
    }

    #[test]
    fn session_completed_does_not_go_negative() {
        let mut poller = BridgeWorkPoller::new("env1".into(), 5);
        poller.session_completed();
        assert_eq!(poller.active_sessions, 0);
    }

    #[test]
    fn register_succeeds() {
        let poller = BridgeWorkPoller::new("env1".into(), 5);
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(poller.register()).is_ok());
    }

    #[test]
    fn spawn_modes() {
        assert_eq!(SpawnMode::SingleSession, SpawnMode::SingleSession);
        assert_ne!(SpawnMode::Worktree, SpawnMode::SameDir);
    }
}
