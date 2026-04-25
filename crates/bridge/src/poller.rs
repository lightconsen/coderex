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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnMode {
    /// Single session in the given directory.
    SingleSession,
    /// Multiple sessions sharing the same directory.
    SameDir,
    /// Create a git worktree for this session.
    Worktree,
    /// Spawn via SSH on a remote host.
    Ssh {
        /// Remote host (user@hostname).
        host: String,
        /// Remote working directory.
        remote_path: String,
        /// SSH private key path (optional, defaults to ssh-agent).
        identity_file: Option<String>,
    },
}

impl SpawnMode {
    /// Convert to the string format expected by the control plane server.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            SpawnMode::SingleSession => "single_session",
            SpawnMode::SameDir => "same_dir",
            SpawnMode::Worktree => "worktree",
            SpawnMode::Ssh { .. } => "ssh",
        }
    }

    /// Parse from the string format used by the control plane server.
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        match s {
            "single_session" => SpawnMode::SingleSession,
            "same_dir" => SpawnMode::SameDir,
            "worktree" => SpawnMode::Worktree,
            _ => SpawnMode::SingleSession,
        }
    }
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

/// Registration request sent to the control plane.
#[derive(Debug, Clone, Serialize)]
struct RegisterRequest {
    env_id: String,
    max_concurrent: usize,
    capabilities: Vec<String>,
}

/// Polls or receives work assignments from the remote control plane.
#[derive(Debug)]
pub struct BridgeWorkPoller {
    /// This environment's identifier.
    pub env_id: String,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Control plane base URL.
    pub control_plane_url: String,
    /// Pending work assignments.
    pending: Vec<WorkAssignment>,
    /// Active session count.
    active_sessions: usize,
}

/// Server-side assignment entry (deserialized from control plane).
#[derive(Debug, Clone, Deserialize)]
struct ServerAssignment {
    assignment_id: String,
    session_id: String,
    working_directory: String,
    prompt: String,
    spawn_mode: String,
}

/// Server response from the assignments endpoint.
#[derive(Debug, Clone, Deserialize)]
struct AssignmentsResponse {
    assignments: Vec<ServerAssignment>,
    #[allow(dead_code)]
    count: usize,
}

impl BridgeWorkPoller {
    /// Create a new work poller.
    #[must_use]
    pub fn new(env_id: String, max_concurrent: usize) -> Self {
        Self {
            env_id,
            max_concurrent,
            control_plane_url: "http://localhost:3000".into(),
            pending: Vec::new(),
            active_sessions: 0,
        }
    }

    /// Create a new work poller with a custom control plane URL.
    #[must_use]
    pub fn with_url(env_id: String, max_concurrent: usize, control_plane_url: String) -> Self {
        Self {
            env_id,
            max_concurrent,
            control_plane_url,
            pending: Vec::new(),
            active_sessions: 0,
        }
    }

    /// Register this environment with the control plane.
    pub async fn register(&self) -> Result<(), String> {
        let client = reqwest::Client::new();
        let url = format!("{}/api/environments/register", self.control_plane_url);
        let body = RegisterRequest {
            env_id: self.env_id.clone(),
            max_concurrent: self.max_concurrent,
            capabilities: vec![
                "worktree".into(),
                "single_session".into(),
                "same_dir".into(),
                "ssh".into(),
            ],
        };
        let response = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("registration request failed: {e}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            Err(format!("registration returned {status}: {text}"))
        }
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

    /// Fetch available assignments from the control plane server.
    ///
    /// Calls `GET /api/environments/{env_id}/assignments` and
    /// enqueues any returned assignments.
    pub async fn fetch_assignments(&mut self) -> Result<usize, String> {
        let client = reqwest::Client::new();
        let url = format!(
            "{}/api/environments/{}/assignments",
            self.control_plane_url, self.env_id
        );
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("fetch assignments failed: {e}"))?;

        if response.status().is_success() {
            let parsed: AssignmentsResponse = response
                .json()
                .await
                .map_err(|e| format!("failed to parse assignments: {e}"))?;

            let count = parsed.assignments.len();
            for server_entry in parsed.assignments {
                self.pending.push(WorkAssignment {
                    assignment_id: server_entry.assignment_id,
                    session_id: server_entry.session_id,
                    working_directory: server_entry.working_directory,
                    prompt: server_entry.prompt,
                    spawn_mode: SpawnMode::from_str(&server_entry.spawn_mode),
                });
            }
            Ok(count)
        } else {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            Err(format!("fetch assignments returned {status}: {text}"))
        }
    }

    /// Report current capacity to the control plane server.
    ///
    /// Calls `POST /api/environments/{env_id}/capacity`.
    pub async fn report_capacity_to_server(&self) -> Result<(), String> {
        let client = reqwest::Client::new();
        let url = format!(
            "{}/api/environments/{}/capacity",
            self.control_plane_url, self.env_id
        );
        let report = self.report_capacity();

        // Build JSON body matching the server's CapacityReport
        let body = serde_json::json!({
            "active_sessions": report.active_sessions,
            "max_concurrent": report.max_concurrent,
            "available_slots": report.available_slots,
        });

        let response = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("report capacity failed: {e}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            Err(format!("report capacity returned {status}: {text}"))
        }
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
    fn register_fails_without_server() {
        let poller = BridgeWorkPoller::new("env1".into(), 5);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(poller.register());
        assert!(result.is_err());
    }

    #[test]
    fn with_url_constructor() {
        let poller =
            BridgeWorkPoller::with_url("env2".into(), 10, "https://control.example:8443".into());
        assert_eq!(poller.control_plane_url, "https://control.example:8443");
        assert_eq!(poller.max_concurrent, 10);
    }

    #[test]
    fn spawn_modes() {
        assert_eq!(SpawnMode::SingleSession, SpawnMode::SingleSession);
        assert_ne!(SpawnMode::Worktree, SpawnMode::SameDir);
    }
}
