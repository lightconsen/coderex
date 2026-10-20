//! Control plane HTTP API for bridge work polling.
//!
//! Serves endpoints that bridge environments use to register,
//! receive work assignments, and report capacity.
//!
//! Endpoints:
//! - `POST /api/environments/register` — Register a bridge environment
//! - `GET /api/environments/{env_id}/assignments` — Pull pending assignments
//! - `POST /api/environments/{env_id}/assignments/{id}/claim` — Claim an assignment
//! - `POST /api/environments/{env_id}/capacity` — Report current capacity
//! - `GET /api/environments` — List all registered environments

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Registration request from a bridge environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// Environment identifier.
    pub env_id: String,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Declared capabilities (e.g., "worktree", "single_session").
    pub capabilities: Vec<String>,
}

/// Registration response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// Whether registration was successful.
    pub ok: bool,
    /// Environment identifier.
    pub env_id: String,
}

/// Where an SSH-spawned session should live.
///
/// Sent beside `spawn_mode: "ssh"`; the mode name alone cannot say which host
/// to reach or where on it to work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    /// Remote host (user@hostname).
    pub host: String,
    /// Remote working directory.
    pub remote_path: String,
    /// SSH private key path (optional, defaults to ssh-agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
}

/// A pending work assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentEntry {
    /// Unique assignment ID.
    pub assignment_id: String,
    /// Session ID to spawn.
    pub session_id: String,
    /// Working directory for the session.
    pub working_directory: String,
    /// Initial prompt.
    pub prompt: String,
    /// Spawn mode string (single_session, same_dir, worktree, ssh).
    pub spawn_mode: String,
    /// SSH target, present when `spawn_mode` is `"ssh"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshTarget>,
    /// Target environment ID (None = unassigned).
    pub assigned_to: Option<String>,
    /// Whether this assignment has been claimed.
    pub claimed: bool,
}

/// Capacity report from an environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityReport {
    /// Active session count.
    pub active_sessions: usize,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Available slots.
    pub available_slots: usize,
}

/// A registered bridge environment.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Registration info.
    pub register: RegisterRequest,
    /// Current capacity report.
    pub capacity: Option<CapacityReport>,
    /// Registered at (Unix timestamp seconds).
    pub registered_at: u64,
}

/// Shared state for the control plane server.
#[derive(Debug, Default)]
pub struct ControlPlaneState {
    /// Registered environments keyed by env_id.
    pub environments: HashMap<String, Environment>,
    /// Pending work assignments keyed by assignment_id.
    pub assignments: HashMap<String, AssignmentEntry>,
}

impl ControlPlaneState {
    /// Register or update an environment.
    pub fn register_env(&mut self, req: RegisterRequest) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.environments.insert(
            req.env_id.clone(),
            Environment {
                register: req,
                capacity: None,
                registered_at: now,
            },
        );
    }

    /// Update capacity for an environment.
    pub fn update_capacity(&mut self, env_id: &str, report: CapacityReport) -> bool {
        if let Some(env) = self.environments.get_mut(env_id) {
            env.capacity = Some(report);
            true
        } else {
            false
        }
    }

    /// Add a new work assignment.
    pub fn add_assignment(&mut self, entry: AssignmentEntry) {
        self.assignments.insert(entry.assignment_id.clone(), entry);
    }

    /// Get all unclaimed assignments for an environment.
    pub fn get_available_assignments(&self, env_id: &str) -> Vec<&AssignmentEntry> {
        self.assignments
            .values()
            .filter(|a| {
                if a.claimed {
                    return false;
                }
                a.assigned_to.as_ref().is_none_or(|id| id == env_id)
            })
            .collect()
    }

    /// Claim an assignment for an environment.
    pub fn claim_assignment(&mut self, assignment_id: &str, env_id: &str) -> bool {
        if let Some(entry) = self.assignments.get_mut(assignment_id) {
            if !entry.claimed {
                entry.claimed = true;
                entry.assigned_to = Some(env_id.to_string());
                return true;
            }
        }
        false
    }

    /// List all registered environment IDs.
    pub fn list_environments(&self) -> Vec<&str> {
        self.environments
            .keys()
            .map(std::string::String::as_str)
            .collect()
    }
}

/// Control plane HTTP server.
#[derive(Debug)]
pub struct ControlPlaneServer {
    /// Port to listen on.
    pub port: u16,
    /// Shared state.
    pub state: Arc<Mutex<ControlPlaneState>>,
}

impl ControlPlaneServer {
    /// Create a new control plane server.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            port,
            state: Arc::new(Mutex::new(ControlPlaneState::default())),
        }
    }

    /// Create a server with pre-existing state.
    #[must_use]
    pub const fn with_state(port: u16, state: Arc<Mutex<ControlPlaneState>>) -> Self {
        Self { port, state }
    }

    /// Start the control plane HTTP server.
    pub async fn run(&self) -> Result<(), String> {
        let addr: SocketAddr = ([0, 0, 0, 0], self.port).into();
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("failed to bind control plane on port {}: {e}", self.port))?;

        info!(
            "Control plane server listening on http://0.0.0.0:{}/api/environments",
            self.port
        );

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!("control plane accept error: {e}");
                    continue;
                }
            };
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                let _ = handle_request(stream, state).await;
            });
        }
    }
}

/// Parse a raw HTTP request and dispatch to the appropriate handler.
async fn handle_request(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<ControlPlaneState>>,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    let (mut reader, mut writer) = stream.into_split();

    // Read HTTP request
    let mut header_buf = [0u8; 4096];
    let n = reader.read(&mut header_buf).await?;
    let request = String::from_utf8_lossy(&header_buf[..n]);
    let first_line = request.lines().next().unwrap_or("");

    // Parse method and path
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return write_response(
            &mut writer,
            400,
            &serde_json::json!({"error": "bad request"}),
        )
        .await;
    }
    let method = parts[0];
    let path = parts[1];

    // Read body if Content-Length present
    let body: String = if let Some(content_len_line) = request
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
    {
        if let Some(len_str) = content_len_line.split(':').nth(1) {
            if let Ok(len) = len_str.trim().parse::<usize>() {
                let header_end = request
                    .find("\r\n\r\n")
                    .or_else(|| request.find("\n\n"))
                    .unwrap_or(n);
                let remaining = n.saturating_sub(header_end);
                let body_len = len.saturating_sub(remaining);
                let mut body_bytes = if remaining > 0 {
                    header_buf[header_end..n].to_vec()
                } else {
                    Vec::new()
                };
                if body_len > 0 {
                    let mut temp = vec![0u8; body_len];
                    if reader.read_exact(&mut temp).await.is_ok() {
                        body_bytes.extend_from_slice(&temp);
                    }
                }
                String::from_utf8_lossy(&body_bytes).to_string()
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let (status, response_body) = route(method, path, &body, &state).await;
    write_response(&mut writer, status, &response_body).await
}

/// Route a request to the appropriate handler.
async fn route(
    method: &str,
    path: &str,
    body: &str,
    state: &Arc<Mutex<ControlPlaneState>>,
) -> (u16, serde_json::Value) {
    // POST /api/environments/register
    if method == "POST" && path == "/api/environments/register" {
        return handle_register(body, state).await;
    }

    // GET /api/environments/{env_id}/assignments
    if method == "GET" {
        if let Some(env_id) = path
            .strip_prefix("/api/environments/")
            .and_then(|p| p.strip_suffix("/assignments"))
        {
            return handle_get_assignments(env_id, state).await;
        }
    }

    // POST /api/environments/{env_id}/assignments/{id}/claim
    if method == "POST" {
        if let Some(rest) = path.strip_prefix("/api/environments/") {
            if let Some(idx) = rest.find("/assignments/") {
                let env_id = &rest[..idx];
                if let Some(after) = rest[idx + "/assignments/".len()..].strip_suffix("/claim") {
                    return handle_claim(env_id, after, state).await;
                }
            }
        }
    }

    // POST /api/environments/{env_id}/capacity
    if method == "POST" {
        if let Some(env_id) = path
            .strip_prefix("/api/environments/")
            .and_then(|p| p.strip_suffix("/capacity"))
        {
            return handle_capacity(env_id, body, state).await;
        }
    }

    // GET /api/environments
    if method == "GET" && path == "/api/environments" {
        return handle_list_environments(state).await;
    }

    (404, serde_json::json!({"error": "not found", "path": path}))
}

/// POST /api/environments/register
async fn handle_register(
    body: &str,
    state: &Arc<Mutex<ControlPlaneState>>,
) -> (u16, serde_json::Value) {
    let req: RegisterRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let env_id = req.env_id.clone();
    state.lock().await.register_env(req);

    (
        200,
        serde_json::json!({
            "ok": true,
            "env_id": env_id,
        }),
    )
}

/// GET /api/environments/{env_id}/assignments
async fn handle_get_assignments(
    env_id: &str,
    state: &Arc<Mutex<ControlPlaneState>>,
) -> (u16, serde_json::Value) {
    let guard = state.lock().await;
    let assignments = guard.get_available_assignments(env_id);
    let entries: Vec<_> = assignments.into_iter().cloned().collect();
    drop(guard);

    (
        200,
        serde_json::json!({
            "env_id": env_id,
            "assignments": entries,
            "count": entries.len(),
        }),
    )
}

/// POST /api/environments/{env_id}/assignments/{id}/claim
async fn handle_claim(
    env_id: &str,
    assignment_id: &str,
    state: &Arc<Mutex<ControlPlaneState>>,
) -> (u16, serde_json::Value) {
    let claimed = state.lock().await.claim_assignment(assignment_id, env_id);
    if claimed {
        (
            200,
            serde_json::json!({
                "ok": true,
                "assignment_id": assignment_id,
                "claimed_by": env_id,
            }),
        )
    } else {
        (
            409,
            serde_json::json!({
                "error": "assignment already claimed or not found",
                "assignment_id": assignment_id,
            }),
        )
    }
}

/// POST /api/environments/{env_id}/capacity
async fn handle_capacity(
    env_id: &str,
    body: &str,
    state: &Arc<Mutex<ControlPlaneState>>,
) -> (u16, serde_json::Value) {
    let report: CapacityReport = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    let updated = state.lock().await.update_capacity(env_id, report);
    if updated {
        (200, serde_json::json!({"ok": true}))
    } else {
        (
            404,
            serde_json::json!({"error": "environment not registered", "env_id": env_id}),
        )
    }
}

/// GET /api/environments
async fn handle_list_environments(
    state: &Arc<Mutex<ControlPlaneState>>,
) -> (u16, serde_json::Value) {
    let response = {
        let guard = state.lock().await;
        let envs = guard.list_environments();
        let count = envs.len();
        let envs_vec: Vec<&str> = envs.into_iter().collect();
        let response = serde_json::json!({
            "environments": envs_vec,
            "count": count,
        });
        drop(guard);
        response
    };
    (200, response)
}

/// Write an HTTP response.
async fn write_response<S: tokio::io::AsyncWrite + Unpin>(
    writer: &mut S,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let body_str = serde_json::to_string(body).unwrap_or_default();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_str}",
        body_str.len(),
    );
    writer.write_all(response.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_register_env() {
        let mut state = ControlPlaneState::default();
        state.register_env(RegisterRequest {
            env_id: "env1".into(),
            max_concurrent: 5,
            capabilities: vec!["worktree".into()],
        });
        assert_eq!(state.list_environments(), vec!["env1"]);
    }

    #[test]
    fn state_update_capacity() {
        let mut state = ControlPlaneState::default();
        state.register_env(RegisterRequest {
            env_id: "env1".into(),
            max_concurrent: 5,
            capabilities: vec![],
        });
        let ok = state.update_capacity(
            "env1",
            CapacityReport {
                active_sessions: 2,
                max_concurrent: 5,
                available_slots: 3,
            },
        );
        assert!(ok);
        assert_eq!(
            state.environments["env1"]
                .capacity
                .as_ref()
                .unwrap()
                .active_sessions,
            2
        );
    }

    #[test]
    fn state_update_capacity_unknown_env() {
        let mut state = ControlPlaneState::default();
        assert!(!state.update_capacity(
            "nope",
            CapacityReport {
                active_sessions: 0,
                max_concurrent: 1,
                available_slots: 1,
            }
        ));
    }

    #[test]
    fn state_add_and_get_assignments() {
        let mut state = ControlPlaneState::default();
        state.add_assignment(AssignmentEntry {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp".into(),
            prompt: "fix it".into(),
            spawn_mode: "single_session".into(),
            ssh: None,
            assigned_to: None,
            claimed: false,
        });
        let assignments = state.get_available_assignments("env1");
        assert_eq!(assignments.len(), 1);
    }

    #[test]
    fn an_ssh_assignment_carries_its_target_on_the_wire() {
        let entry = AssignmentEntry {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/srv/work".into(),
            prompt: "fix it".into(),
            spawn_mode: "ssh".into(),
            ssh: Some(SshTarget {
                host: "deploy@remote.example".into(),
                remote_path: "/srv/coderex".into(),
                identity_file: Some("/home/deploy/.ssh/id_ed25519".into()),
            }),
            assigned_to: None,
            claimed: false,
        };

        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["spawn_mode"], "ssh");
        assert_eq!(json["ssh"]["host"], "deploy@remote.example");
        assert_eq!(json["ssh"]["remote_path"], "/srv/coderex");
        assert_eq!(json["ssh"]["identity_file"], "/home/deploy/.ssh/id_ed25519");
    }

    #[test]
    fn state_claim_assignment() {
        let mut state = ControlPlaneState::default();
        state.add_assignment(AssignmentEntry {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp".into(),
            prompt: "fix it".into(),
            spawn_mode: "single_session".into(),
            ssh: None,
            assigned_to: None,
            claimed: false,
        });
        assert!(state.claim_assignment("a1", "env1"));
        assert!(!state.claim_assignment("a1", "env2")); // already claimed
        let entry = &state.assignments["a1"];
        assert!(entry.claimed);
        assert_eq!(entry.assigned_to, Some("env1".into()));
    }

    #[test]
    fn get_assignments_only_available() {
        let mut state = ControlPlaneState::default();
        state.add_assignment(AssignmentEntry {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp".into(),
            prompt: "fix".into(),
            spawn_mode: "single_session".into(),
            ssh: None,
            assigned_to: None,
            claimed: false,
        });
        state.add_assignment(AssignmentEntry {
            assignment_id: "a2".into(),
            session_id: "s2".into(),
            working_directory: "/tmp".into(),
            prompt: "fix2".into(),
            spawn_mode: "single_session".into(),
            ssh: None,
            assigned_to: None,
            claimed: true, // already claimed
        });
        let available = state.get_available_assignments("env1");
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].assignment_id, "a1");
    }

    #[tokio::test]
    async fn handle_register_json() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        let body = serde_json::to_string(&RegisterRequest {
            env_id: "test-env".into(),
            max_concurrent: 10,
            capabilities: vec!["worktree".into(), "ssh".into()],
        })
        .unwrap();
        let (status, resp) = handle_register(&body, &state).await;
        assert_eq!(status, 200);
        assert_eq!(resp["env_id"], "test-env");
        assert_eq!(resp["ok"], true);
    }

    #[tokio::test]
    async fn handle_register_invalid_json() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        let (status, _) = handle_register("not-json", &state).await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn handle_get_assignments_empty() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        let (status, resp) = handle_get_assignments("env1", &state).await;
        assert_eq!(status, 200);
        assert_eq!(resp["count"], 0);
    }

    #[tokio::test]
    async fn handle_claim_success() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        state.lock().await.add_assignment(AssignmentEntry {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp".into(),
            prompt: "fix".into(),
            spawn_mode: "single_session".into(),
            ssh: None,
            assigned_to: None,
            claimed: false,
        });
        let (status, resp) = handle_claim("env1", "a1", &state).await;
        assert_eq!(status, 200);
        assert_eq!(resp["ok"], true);
    }

    #[tokio::test]
    async fn handle_claim_already_claimed() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        state.lock().await.add_assignment(AssignmentEntry {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp".into(),
            prompt: "fix".into(),
            spawn_mode: "single_session".into(),
            ssh: None,
            assigned_to: None,
            claimed: true,
        });
        let (status, _) = handle_claim("env1", "a1", &state).await;
        assert_eq!(status, 409);
    }

    #[tokio::test]
    async fn handle_capacity_update() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        state.lock().await.register_env(RegisterRequest {
            env_id: "env1".into(),
            max_concurrent: 5,
            capabilities: vec![],
        });
        let body = serde_json::to_string(&CapacityReport {
            active_sessions: 2,
            max_concurrent: 5,
            available_slots: 3,
        })
        .unwrap();
        let (status, _) = handle_capacity("env1", &body, &state).await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn handle_capacity_unknown_env() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        let body = serde_json::to_string(&CapacityReport {
            active_sessions: 0,
            max_concurrent: 1,
            available_slots: 1,
        })
        .unwrap();
        let (status, _) = handle_capacity("nope", &body, &state).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn list_environments_returns_all() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        state.lock().await.register_env(RegisterRequest {
            env_id: "env-a".into(),
            max_concurrent: 1,
            capabilities: vec![],
        });
        state.lock().await.register_env(RegisterRequest {
            env_id: "env-b".into(),
            max_concurrent: 2,
            capabilities: vec![],
        });
        let (status, resp) = handle_list_environments(&state).await;
        assert_eq!(status, 200);
        assert_eq!(resp["count"], 2);
    }

    #[tokio::test]
    async fn route_404() {
        let state = Arc::new(Mutex::new(ControlPlaneState::default()));
        let (status, resp) = route("GET", "/nonexistent", "", &state).await;
        assert_eq!(status, 404);
        assert_eq!(resp["error"], "not found");
    }
}
