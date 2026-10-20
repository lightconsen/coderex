//! Bridge work poller — environment registration and capacity reporting.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Where an SSH-spawned session should live.
///
/// Carried beside the `"ssh"` spawn mode string on the wire, because the mode
/// name alone does not say which host to reach or where on it to work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    /// Remote host (user@hostname).
    pub host: String,
    /// Remote working directory.
    pub remote_path: String,
    /// SSH private key path (optional, defaults to ssh-agent).
    #[serde(default)]
    pub identity_file: Option<String>,
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
    pub const fn as_str(&self) -> &str {
        match self {
            Self::SingleSession => "single_session",
            Self::SameDir => "same_dir",
            Self::Worktree => "worktree",
            Self::Ssh { .. } => "ssh",
        }
    }

    /// Parse from the string format used by the control plane server.
    ///
    /// An `"ssh"` mode without a target cannot name a host or directory, so it
    /// falls back to a local single session rather than spawning an SSH
    /// session to `""`. Use [`SpawnMode::from_str_with_ssh`] when the
    /// assignment carries its target.
    // Named like the trait method on purpose: this is the assignment's wire
    // vocabulary, not a `FromStr` conversion.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        Self::from_str_with_ssh(s, None)
    }

    /// Parse a spawn mode string together with the optional SSH target the
    /// control plane supplies beside it.
    #[must_use]
    pub fn from_str_with_ssh(s: &str, ssh: Option<SshTarget>) -> Self {
        match (s, ssh) {
            ("same_dir", _) => Self::SameDir,
            ("worktree", _) => Self::Worktree,
            ("ssh", Some(target)) => Self::Ssh {
                host: target.host,
                remote_path: target.remote_path,
                identity_file: target.identity_file,
            },
            // `"ssh"` with no target, and every unknown spelling, degrades to
            // the local mode instead of inventing an unreachable host.
            _ => Self::SingleSession,
        }
    }
}

/// The outcome of asking the control plane to claim an assignment.
///
/// A claim is a compare-and-set on the control plane: only one worker gets to
/// run an assignment. `Lost` is a normal race, not an error, and the worker
/// skips the assignment without recording a session failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The assignment belongs to this environment and should be run.
    Claimed,
    /// The assignment was already claimed, no longer exists, or is otherwise
    /// not runnable here. Skip it and release its slot.
    Lost,
}

/// Default path template for claiming an assignment.
///
/// `{env_id}` and `{assignment_id}` are substituted by
/// [`HttpControlPlaneClient`]. The template matches the control-plane server
/// in `coderex-server`; deployments with a different route can override it
/// with [`HttpControlPlaneClient::with_claim_path`].
pub const DEFAULT_CLAIM_PATH_TEMPLATE: &str =
    "/api/environments/{env_id}/assignments/{assignment_id}/claim";

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

/// The control-plane calls a bridge worker needs.
///
/// Implemented over HTTP by [`HttpControlPlaneClient`]. Tests implement it
/// directly so a worker can be exercised without a live control plane.
#[async_trait::async_trait]
pub trait ControlPlaneClient: Send + Sync + std::fmt::Debug {
    /// Register this environment.
    ///
    /// Returns the worker epoch the control plane assigned. The epoch fences
    /// this worker's later write-backs: a stale worker cannot overwrite the
    /// state of the one that replaced it. [`HttpControlPlaneClient`] reads
    /// `worker_epoch` (or `workerEpoch`) from the registration response; `0`
    /// means the control plane did not report an epoch, and write-backs are
    /// disabled until one is known.
    async fn register(&self, env_id: &str, max_concurrent: usize) -> Result<u64, String>;

    /// Fetch work assignments for this environment.
    async fn fetch_assignments(&self, env_id: &str) -> Result<Vec<WorkAssignment>, String>;

    /// Claim an assignment before running it.
    ///
    /// `Ok(ClaimOutcome::Lost)` means the assignment was already claimed (or
    /// no longer exists): the worker must skip it rather than run it twice.
    /// `Err` means the claim could not be completed and the session should be
    /// failed.
    async fn claim_assignment(
        &self,
        env_id: &str,
        assignment_id: &str,
    ) -> Result<ClaimOutcome, String>;

    /// Report current capacity for this environment.
    async fn report_capacity(&self, env_id: &str, report: &CapacityReport) -> Result<(), String>;
}

/// HTTP implementation of [`ControlPlaneClient`].
#[derive(Debug, Clone)]
pub struct HttpControlPlaneClient {
    /// Control-plane base URL, without a trailing slash.
    base_url: String,
    /// Optional bearer token sent with every control-plane request.
    auth_token: Option<String>,
    /// Path template used for claims (`{env_id}`, `{assignment_id}`).
    claim_path: String,
}

impl HttpControlPlaneClient {
    /// Create a client for the given control-plane base URL.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth_token: None,
            claim_path: DEFAULT_CLAIM_PATH_TEMPLATE.to_string(),
        }
    }

    /// Send a bearer token with every request.
    #[must_use]
    pub fn with_auth(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Override the claim path template.
    ///
    /// The template must contain `{assignment_id}`; `{env_id}` is substituted
    /// when present. A deployment whose control plane exposes
    /// `/api/assignments/{assignment_id}/claim` can supply that spelling.
    #[must_use]
    pub fn with_claim_path(mut self, template: impl Into<String>) -> Self {
        self.claim_path = template.into();
        self
    }

    /// The claim path for one assignment.
    ///
    /// The braces are placeholders for `replace`, not formatting arguments.
    #[allow(clippy::literal_string_with_formatting_args)]
    #[must_use]
    pub fn claim_url(&self, env_id: &str, assignment_id: &str) -> String {
        let path = self
            .claim_path
            .replace("{env_id}", env_id)
            .replace("{assignment_id}", assignment_id);
        format!("{}{}", self.base_url, path)
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        let request = reqwest::Client::new().request(method, url);
        match self.auth_token {
            Some(ref token) => request.bearer_auth(token),
            None => request,
        }
    }
}

/// Pull the worker epoch out of a registration response body.
///
/// The reference server spells the field `worker_epoch`; `workerEpoch` is
/// accepted for control planes that use the camelCase spelling. A body that
/// is not JSON, or does not carry the field as a number or numeric string,
/// means no epoch was assigned: the worker stays at `0` and write-backs are
/// disabled rather than sent under a guessed epoch.
fn worker_epoch_from_response(body: &str) -> u64 {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return 0;
    };

    ["worker_epoch", "workerEpoch"]
        .iter()
        .find_map(|key| {
            value.get(*key).and_then(|epoch| {
                epoch
                    .as_u64()
                    .or_else(|| epoch.as_str().and_then(|s| s.parse().ok()))
            })
        })
        .unwrap_or(0)
}

/// Whether a claim response means "this assignment is not ours to run".
///
/// The control-plane server answers 409 for an already-claimed (or missing)
/// assignment. 404 and 422 are accepted too, so a load balancer, a proxy, or a
/// compatible server that spells "gone"/"not claimable" differently still
/// results in a skip rather than an error.
const fn claim_status_is_lost(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 404 | 409 | 422)
}

#[async_trait::async_trait]
impl ControlPlaneClient for HttpControlPlaneClient {
    async fn register(&self, env_id: &str, max_concurrent: usize) -> Result<u64, String> {
        let url = format!("{}/api/environments/register", self.base_url);
        let body = RegisterRequest {
            env_id: env_id.to_string(),
            max_concurrent,
            capabilities: vec![
                "worktree".into(),
                "single_session".into(),
                "same_dir".into(),
                "ssh".into(),
            ],
        };

        let response = self
            .request(reqwest::Method::POST, url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("registration request failed: {e}"))?;

        if response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            Ok(worker_epoch_from_response(&body))
        } else {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            Err(format!("registration returned {status}: {text}"))
        }
    }

    async fn fetch_assignments(&self, env_id: &str) -> Result<Vec<WorkAssignment>, String> {
        let url = format!("{}/api/environments/{env_id}/assignments", self.base_url);
        let response = self
            .request(reqwest::Method::GET, url)
            .send()
            .await
            .map_err(|e| format!("fetch assignments failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("fetch assignments returned {status}: {text}"));
        }

        let parsed: AssignmentsResponse = response
            .json()
            .await
            .map_err(|e| format!("failed to parse assignments: {e}"))?;

        Ok(parsed
            .assignments
            .into_iter()
            .map(ServerAssignment::into_work_assignment)
            .collect())
    }

    async fn claim_assignment(
        &self,
        env_id: &str,
        assignment_id: &str,
    ) -> Result<ClaimOutcome, String> {
        let url = self.claim_url(env_id, assignment_id);
        let response = self
            .request(reqwest::Method::POST, url)
            .send()
            .await
            .map_err(|e| format!("claim assignment failed: {e}"))?;

        let status = response.status();
        if status.is_success() {
            return Ok(ClaimOutcome::Claimed);
        }
        if claim_status_is_lost(status) {
            return Ok(ClaimOutcome::Lost);
        }

        let text = response.text().await.unwrap_or_default();
        Err(format!("claim assignment returned {status}: {text}"))
    }

    async fn report_capacity(&self, env_id: &str, report: &CapacityReport) -> Result<(), String> {
        let url = format!("{}/api/environments/{env_id}/capacity", self.base_url);
        let body = serde_json::json!({
            "active_sessions": report.active_sessions,
            "max_concurrent": report.max_concurrent,
            "available_slots": report.available_slots,
        });

        let response = self
            .request(reqwest::Method::POST, url)
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

/// Polls or receives work assignments from the remote control plane.
#[derive(Debug)]
pub struct BridgeWorkPoller {
    /// This environment's identifier.
    pub env_id: String,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Control plane base URL.
    pub control_plane_url: String,
    /// Client used for registration, assignment fetch, and capacity reports.
    client: Arc<dyn ControlPlaneClient>,
    /// Pending work assignments.
    pending: Vec<WorkAssignment>,
    /// Active session count.
    active_sessions: usize,
    /// Epoch the control plane assigned at registration, or `0` when it did
    /// not report one. Write-backs are fenced by this value.
    worker_epoch: AtomicU64,
}

/// Server-side assignment entry (deserialized from control plane).
#[derive(Debug, Clone, Deserialize)]
struct ServerAssignment {
    assignment_id: String,
    session_id: String,
    working_directory: String,
    prompt: String,
    spawn_mode: String,
    /// SSH target, present when `spawn_mode` is `"ssh"`.
    #[serde(default)]
    ssh: Option<SshTarget>,
}

impl ServerAssignment {
    /// Turn the control-plane spelling into a runnable assignment, carrying
    /// the SSH target (when any) into [`SpawnMode::Ssh`].
    fn into_work_assignment(self) -> WorkAssignment {
        WorkAssignment {
            assignment_id: self.assignment_id,
            session_id: self.session_id,
            working_directory: self.working_directory,
            prompt: self.prompt,
            spawn_mode: SpawnMode::from_str_with_ssh(&self.spawn_mode, self.ssh),
        }
    }
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
        Self::with_url(env_id, max_concurrent, "http://localhost:3000".to_string())
    }

    /// Create a new work poller with a custom control plane URL.
    #[must_use]
    pub fn with_url(env_id: String, max_concurrent: usize, control_plane_url: String) -> Self {
        let client = Arc::new(HttpControlPlaneClient::new(control_plane_url.clone()));
        Self::with_client(env_id, max_concurrent, control_plane_url, client)
    }

    /// Create a work poller backed by an injected control-plane client.
    #[must_use]
    pub fn with_client(
        env_id: String,
        max_concurrent: usize,
        control_plane_url: String,
        client: Arc<dyn ControlPlaneClient>,
    ) -> Self {
        Self {
            env_id,
            max_concurrent,
            control_plane_url,
            client,
            pending: Vec::new(),
            active_sessions: 0,
            worker_epoch: AtomicU64::new(0),
        }
    }

    /// Register this environment with the control plane.
    ///
    /// The epoch the control plane returns is remembered for write-backs.
    pub async fn register(&self) -> Result<(), String> {
        let epoch = self
            .client
            .register(&self.env_id, self.max_concurrent)
            .await?;
        self.worker_epoch.store(epoch, Ordering::SeqCst);
        Ok(())
    }

    /// The epoch registration assigned this worker, or `0` before
    /// registration (or when the control plane did not report one).
    #[must_use]
    pub fn worker_epoch(&self) -> u64 {
        self.worker_epoch.load(Ordering::SeqCst)
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
        if self.active_sessions < self.max_concurrent && !self.pending.is_empty() {
            self.active_sessions += 1;
            return Some(self.pending.remove(0));
        }
        None
    }

    /// Mark a session as completed, freeing capacity.
    pub const fn session_completed(&mut self) {
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
        let assignments = self.client.fetch_assignments(&self.env_id).await?;
        let count = assignments.len();
        self.pending.extend(assignments);
        Ok(count)
    }

    /// Claim an assignment before running it.
    ///
    /// Calls the control plane's claim route (by default
    /// `POST /api/environments/{env_id}/assignments/{assignment_id}/claim`).
    pub async fn claim_assignment(
        &self,
        assignment: &WorkAssignment,
    ) -> Result<ClaimOutcome, String> {
        self.client
            .claim_assignment(&self.env_id, &assignment.assignment_id)
            .await
    }

    /// Report current capacity to the control plane server.
    ///
    /// Calls `POST /api/environments/{env_id}/capacity`.
    pub async fn report_capacity_to_server(&self) -> Result<(), String> {
        let report = self.report_capacity();
        self.client.report_capacity(&self.env_id, &report).await
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

    #[derive(Debug, Default)]
    struct FakeClient {
        registrations: std::sync::Mutex<Vec<(String, usize)>>,
        assignments: std::sync::Mutex<Vec<WorkAssignment>>,
        capacities: std::sync::Mutex<Vec<(String, CapacityReport)>>,
        claims: std::sync::Mutex<Vec<(String, String)>>,
        /// Epoch returned by registration.
        worker_epoch: u64,
        /// Result returned by the next claim; `None` means `Claimed`.
        claim_outcome: std::sync::Mutex<Option<Result<ClaimOutcome, String>>>,
    }

    #[async_trait::async_trait]
    impl ControlPlaneClient for FakeClient {
        async fn register(&self, env_id: &str, max_concurrent: usize) -> Result<u64, String> {
            self.registrations
                .lock()
                .unwrap()
                .push((env_id.to_string(), max_concurrent));
            Ok(self.worker_epoch)
        }

        async fn fetch_assignments(&self, _env_id: &str) -> Result<Vec<WorkAssignment>, String> {
            Ok(std::mem::take(&mut *self.assignments.lock().unwrap()))
        }

        async fn claim_assignment(
            &self,
            env_id: &str,
            assignment_id: &str,
        ) -> Result<ClaimOutcome, String> {
            self.claims
                .lock()
                .unwrap()
                .push((env_id.to_string(), assignment_id.to_string()));
            self.claim_outcome
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(ClaimOutcome::Claimed))
        }

        async fn report_capacity(
            &self,
            env_id: &str,
            report: &CapacityReport,
        ) -> Result<(), String> {
            self.capacities
                .lock()
                .unwrap()
                .push((env_id.to_string(), report.clone()));
            Ok(())
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

    #[tokio::test]
    async fn injected_client_drives_register_fetch_and_capacity() {
        let client = Arc::new(FakeClient::default());
        client.assignments.lock().unwrap().push(sample_assignment());

        let mut poller = BridgeWorkPoller::with_client(
            "env1".into(),
            2,
            "http://unused.invalid".into(),
            client.clone(),
        );

        poller.register().await.unwrap();
        assert_eq!(
            client.registrations.lock().unwrap().as_slice(),
            &[("env1".to_string(), 2)]
        );

        assert_eq!(poller.fetch_assignments().await.unwrap(), 1);
        assert!(poller.has_pending());

        let taken = poller.take_assignment().unwrap();
        assert_eq!(taken.session_id, "s1");

        assert_eq!(
            poller.claim_assignment(&taken).await.unwrap(),
            ClaimOutcome::Claimed
        );
        assert_eq!(
            client.claims.lock().unwrap().as_slice(),
            &[("env1".to_string(), "a1".to_string())]
        );

        poller.report_capacity_to_server().await.unwrap();
        let capacities = client.capacities.lock().unwrap();
        assert_eq!(capacities.len(), 1);
        assert_eq!(capacities[0].0, "env1");
        assert_eq!(capacities[0].1.active_sessions, 1);
        assert_eq!(capacities[0].1.available_slots, 1);
    }

    #[tokio::test]
    async fn registration_stores_the_epoch_the_control_plane_returns() {
        let client = Arc::new(FakeClient {
            worker_epoch: 11,
            ..FakeClient::default()
        });
        let poller =
            BridgeWorkPoller::with_client("env1".into(), 1, "http://unused.invalid".into(), client);

        assert_eq!(
            poller.worker_epoch(),
            0,
            "unregistered workers have no epoch"
        );
        poller.register().await.unwrap();
        assert_eq!(poller.worker_epoch(), 11);
    }

    #[test]
    fn registration_response_epoch_spellings_are_parsed() {
        assert_eq!(worker_epoch_from_response(r#"{"worker_epoch":42}"#), 42);
        assert_eq!(worker_epoch_from_response(r#"{"workerEpoch":7}"#), 7);
        assert_eq!(worker_epoch_from_response(r#"{"worker_epoch":"9"}"#), 9);
        // A server that reports no epoch, or no JSON at all, leaves the worker
        // unregistered rather than guessing one.
        assert_eq!(worker_epoch_from_response("{}"), 0);
        assert_eq!(worker_epoch_from_response(""), 0);
        assert_eq!(worker_epoch_from_response("not json"), 0);
    }

    #[tokio::test]
    async fn claim_lost_and_claim_failure_are_distinct() {
        let client = Arc::new(FakeClient::default());
        let poller = BridgeWorkPoller::with_client(
            "env1".into(),
            2,
            "http://unused.invalid".into(),
            client.clone(),
        );
        let assignment = sample_assignment();

        *client.claim_outcome.lock().unwrap() = Some(Ok(ClaimOutcome::Lost));
        assert_eq!(
            poller.claim_assignment(&assignment).await.unwrap(),
            ClaimOutcome::Lost
        );

        *client.claim_outcome.lock().unwrap() = Some(Err("control plane unavailable".into()));
        let error = poller.claim_assignment(&assignment).await.unwrap_err();
        assert!(error.contains("unavailable"), "{error}");
    }

    #[test]
    fn claim_path_is_explicit_and_overridable() {
        let client = HttpControlPlaneClient::new("http://control.example/");
        assert_eq!(
            client.claim_url("env-1", "asn-1"),
            "http://control.example/api/environments/env-1/assignments/asn-1/claim"
        );

        let client = client.with_claim_path("/api/assignments/{assignment_id}/claim");
        assert_eq!(
            client.claim_url("env-1", "asn-1"),
            "http://control.example/api/assignments/asn-1/claim"
        );
    }

    #[test]
    fn lost_claim_statuses_are_skipped() {
        for status in [404, 409, 422] {
            let status = reqwest::StatusCode::from_u16(status).unwrap();
            assert!(claim_status_is_lost(status), "{status} should be lost");
        }
        for status in [400, 401, 403, 500, 503] {
            let status = reqwest::StatusCode::from_u16(status).unwrap();
            assert!(
                !claim_status_is_lost(status),
                "{status} should surface as a failure"
            );
        }
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

    #[test]
    fn an_ssh_assignment_maps_to_ssh_with_its_fields() {
        let json = r#"{
            "assignments": [{
                "assignment_id": "a1",
                "session_id": "s1",
                "working_directory": "/srv/work",
                "prompt": "Fix the bug",
                "spawn_mode": "ssh",
                "ssh": {
                    "host": "deploy@remote.example",
                    "remote_path": "/srv/coderex",
                    "identity_file": "/home/deploy/.ssh/id_ed25519"
                }
            }],
            "count": 1
        }"#;

        let parsed: AssignmentsResponse = serde_json::from_str(json).unwrap();
        let assignment = parsed
            .assignments
            .into_iter()
            .next()
            .unwrap()
            .into_work_assignment();

        assert_eq!(
            assignment.spawn_mode,
            SpawnMode::Ssh {
                host: "deploy@remote.example".into(),
                remote_path: "/srv/coderex".into(),
                identity_file: Some("/home/deploy/.ssh/id_ed25519".into()),
            }
        );
    }

    #[test]
    fn ssh_without_a_target_does_not_invent_one() {
        // A mode name on its own cannot say where to connect; it must not
        // become an SSH spawn to an empty host.
        assert_eq!(
            SpawnMode::from_str_with_ssh("ssh", None),
            SpawnMode::SingleSession
        );
    }
}
