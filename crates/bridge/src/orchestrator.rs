//! Bridge orchestrator — integrates all bridge modules into a cohesive system.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::archive::{BridgeArchiveWriter, BridgeCleanupManager, UsageStats};
use crate::heartbeat::{
    HeartbeatConfig, HeartbeatLoop, HeartbeatStatus, SessionRecoveryState, WsReconnector,
};
use crate::permission::{BridgePermissionRouter, PermissionResponse};
use crate::poller::{BridgeWorkPoller, SpawnMode, WorkAssignment};
use crate::spawner::{BridgeSessionSpawner, SpawnConfig, SpawnResult};
use crate::state::{BridgeSessionState, BridgeSessionStateMachine};
use crate::transport::Transport;

/// Configuration for the bridge system.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Environment identifier.
    pub env_id: String,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Control plane base URL.
    pub control_plane_url: String,
    /// Git repository root.
    pub repo_root: PathBuf,
    /// Base directory for worktrees.
    pub worktree_base: PathBuf,
    /// Base directory for session data.
    pub session_base: PathBuf,
    /// Base directory for archives.
    pub archive_base: PathBuf,
    /// Heartbeat configuration.
    pub heartbeat: HeartbeatConfig,
}

/// A running session tracked by the bridge.
#[derive(Debug)]
pub struct ManagedSession {
    /// Session state machine.
    pub state_machine: BridgeSessionStateMachine,
    /// Working directory for the session.
    pub work_dir: PathBuf,
    /// Whether a worktree was created.
    pub worktree_created: bool,
}

/// Bridge events emitted during operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum BridgeEvent {
    /// A session was spawned.
    SessionSpawned {
        session_id: String,
        work_dir: String,
    },
    /// A session completed successfully.
    SessionCompleted { session_id: String },
    /// A session failed.
    SessionFailed { session_id: String, reason: String },
    /// A permission request was routed.
    PermissionRequested {
        request_id: String,
        session_id: String,
        tool: String,
    },
    /// Permission was granted/denied.
    PermissionResolved { request_id: String, allowed: bool },
    /// Transport disconnected and is reconnecting.
    TransportReconnecting { attempt: usize },
    /// Transport reconnected successfully.
    TransportReconnected,
    /// Heartbeat status changed.
    HeartbeatChanged { status: String },
    /// Session archived and cleaned up.
    SessionArchived { session_id: String },
}

/// The main bridge orchestrator.
///
/// Owns and wires together all bridge modules:
/// - Transport for message routing
/// - HeartbeatLoop + WsReconnector for connection health
/// - BridgeWorkPoller for work assignments
/// - BridgeSessionSpawner for session creation
/// - BridgePermissionRouter for tool call permissions
/// - BridgeSessionStateMachine for lifecycle tracking
/// - BridgeArchiveWriter + BridgeCleanupManager for session archival
pub struct Bridge {
    /// Bridge configuration.
    _config: BridgeConfig,
    /// Transport layer.
    transport: Option<Box<dyn Transport>>,
    /// Heartbeat loop for connection monitoring.
    heartbeat_loop: Option<HeartbeatLoop>,
    /// Reconnection logic for transport disconnects.
    reconnector: WsReconnector,
    /// Work poller for receiving assignments.
    poller: BridgeWorkPoller,
    /// Session spawner for worktree/directory creation.
    spawner: BridgeSessionSpawner,
    /// Permission router for tool call authorization.
    permission_router: BridgePermissionRouter,
    /// Archive writer for session snapshots.
    archive_writer: BridgeArchiveWriter,
    /// Cleanup manager for resource teardown.
    cleanup_manager: BridgeCleanupManager,
    /// Active sessions tracked by state machine.
    pub sessions: std::collections::HashMap<String, ManagedSession>,
    /// Event broadcast channel for observers.
    event_tx: tokio::sync::broadcast::Sender<BridgeEvent>,
    /// Whether the bridge is currently running.
    running: bool,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("sessions", &self.sessions.len())
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl Bridge {
    /// Create a new bridge from the given configuration and transport.
    pub fn new(config: BridgeConfig, transport: Box<dyn Transport>) -> Self {
        let poller = BridgeWorkPoller::with_url(
            config.env_id.clone(),
            config.max_concurrent,
            config.control_plane_url.clone(),
        );

        let spawner = BridgeSessionSpawner::new(
            config.repo_root.clone(),
            config.worktree_base.clone(),
            config.session_base.clone(),
        );

        let archive_writer = BridgeArchiveWriter::new(config.archive_base.clone());

        let heartbeat_loop = HeartbeatLoop::new(config.heartbeat);
        let reconnector = WsReconnector::default();

        let (event_tx, _) = tokio::sync::broadcast::channel(256);

        Self {
            _config: config.clone(),
            transport: Some(transport),
            heartbeat_loop: Some(heartbeat_loop),
            reconnector,
            poller,
            spawner,
            permission_router: BridgePermissionRouter::new(),
            archive_writer,
            cleanup_manager: BridgeCleanupManager::new(
                config.worktree_base.clone(),
                config.session_base.clone(),
            ),
            sessions: std::collections::HashMap::new(),
            event_tx,
            running: false,
        }
    }

    /// Subscribe to bridge events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<BridgeEvent> {
        self.event_tx.subscribe()
    }

    /// Broadcast an event to all subscribers.
    pub fn emit(&self, event: BridgeEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Connect the transport layer.
    pub async fn connect(&mut self) -> Result<(), String> {
        let transport = self.transport.as_mut().ok_or("no transport configured")?;
        transport.connect().await.map_err(|e| e.to_string())?;
        self.running = true;
        Ok(())
    }

    /// Disconnect the transport layer.
    pub async fn disconnect(&mut self) -> Result<(), String> {
        if let Some(transport) = self.transport.as_mut() {
            transport.disconnect().await.map_err(|e| e.to_string())?;
        }
        if let Some(ref heartbeat) = self.heartbeat_loop {
            heartbeat.stop();
        }
        self.running = false;
        Ok(())
    }

    /// Check if the bridge is currently running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Register this environment with the control plane.
    pub async fn register(&self) -> Result<(), String> {
        self.poller.register().await
    }

    /// Submit a permission response for a pending request.
    pub fn respond_to_permission(
        &mut self,
        request_id: &str,
        response: PermissionResponse,
    ) -> bool {
        let resolved = self.permission_router.respond(request_id, response);
        if resolved {
            if let Some(resp) = self.permission_router.get_response(request_id) {
                let allowed = matches!(resp, PermissionResponse::Allow { .. });
                self.emit(BridgeEvent::PermissionResolved {
                    request_id: request_id.to_string(),
                    allowed,
                });
            }
        }
        resolved
    }

    /// Get the heartbeat status.
    #[must_use]
    pub fn heartbeat_status(&self) -> HeartbeatStatus {
        if let Some(ref hb) = self.heartbeat_loop {
            let state = hb.heartbeat();
            let guard = state.lock().unwrap();
            guard.status()
        } else {
            HeartbeatStatus::Lost
        }
    }

    /// List active session IDs.
    #[must_use]
    pub fn active_sessions(&self) -> Vec<&str> {
        self.sessions
            .iter()
            .filter(|(_, s)| !s.state_machine.is_terminal())
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Check if a session is tracked by the bridge.
    #[must_use]
    pub fn has_session(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// Get the state of a specific session.
    pub fn session_state(&self, session_id: &str) -> Option<&BridgeSessionState> {
        self.sessions
            .get(session_id)
            .map(|s| &s.state_machine.current)
    }

    /// Get the number of active sessions.
    #[must_use]
    pub fn active_session_count(&self) -> usize {
        self.active_sessions().len()
    }

    /// Get the poller's pending assignment count.
    #[must_use]
    pub fn pending_assignment_count(&self) -> usize {
        self.poller.pending_count()
    }

    /// Receive a work assignment (delegated to the poller).
    pub fn receive_assignment(&mut self, assignment: WorkAssignment) {
        self.poller.receive_assignment(assignment);
    }

    /// Take the next pending assignment (delegated to the poller).
    pub fn take_assignment(&mut self) -> Option<WorkAssignment> {
        self.poller.take_assignment()
    }

    /// Get the permission router's pending count for a session.
    #[must_use]
    pub fn pending_permission_count(&self, session_id: &str) -> usize {
        self.permission_router.pending_for_session(session_id).len()
    }

    /// Get the current transport connection ID.
    pub fn connection_id(&self) -> Option<&str> {
        self.transport.as_ref().map(|t| t.connection_id())
    }

    /// Spawn a session from a work assignment.
    pub async fn spawn_session(
        &mut self,
        assignment: &WorkAssignment,
    ) -> Result<SpawnResult, String> {
        let session_id = &assignment.session_id;

        let config = SpawnConfig {
            session_id: session_id.clone(),
            work_dir: PathBuf::from(&assignment.working_directory),
            mode: assignment.spawn_mode.clone(),
            branch: match assignment.spawn_mode {
                SpawnMode::Worktree => Some("HEAD".into()),
                SpawnMode::Ssh { .. } => None,
                _ => None,
            },
        };

        let result = self.spawner.spawn(config).await?;

        self.sessions.insert(
            session_id.clone(),
            ManagedSession {
                state_machine: BridgeSessionStateMachine::new(session_id.clone()),
                work_dir: result.work_dir.clone(),
                worktree_created: result.worktree_created,
            },
        );

        self.emit(BridgeEvent::SessionSpawned {
            session_id: session_id.clone(),
            work_dir: result.work_dir.display().to_string(),
        });

        Ok(result)
    }

    /// Complete a session (archive + cleanup).
    pub async fn complete_session(
        &mut self,
        session_id: &str,
        usage: UsageStats,
    ) -> Result<(), String> {
        // Drive session through required states: assigned → preparing → spawning → running → archiving
        if let Some(session) = self.sessions.get_mut(session_id) {
            if matches!(session.state_machine.current, BridgeSessionState::Assigned) {
                session
                    .state_machine
                    .transition(BridgeSessionState::Preparing)
                    .map_err(|e| format!("state transition failed: {e}"))?;
                session
                    .state_machine
                    .transition(BridgeSessionState::Spawning)
                    .map_err(|e| format!("state transition failed: {e}"))?;
                session
                    .state_machine
                    .transition(BridgeSessionState::Running)
                    .map_err(|e| format!("state transition failed: {e}"))?;
            }
            session
                .state_machine
                .transition(BridgeSessionState::Archiving)
                .map_err(|e| format!("state transition failed: {e}"))?;
        }

        // Archive the session
        let archive = self
            .archive_writer
            .archive(
                session_id,
                "", // transcript content — would come from transport
                usage,
                "completed",
            )
            .await?;

        self.emit(BridgeEvent::SessionArchived {
            session_id: session_id.to_string(),
        });

        // Transition to cleaning up
        if let Some(session) = self.sessions.get_mut(session_id) {
            session
                .state_machine
                .transition(BridgeSessionState::CleaningUp)
                .map_err(|e| format!("state transition failed: {e}"))?;
        }

        // Clean up resources
        let _report = self.cleanup_manager.cleanup(session_id)?;

        // Clean up worktree if created
        if let Some(session) = self.sessions.remove(session_id) {
            if session.worktree_created {
                let _ = self.spawner.cleanup_worktree(session_id);
            }
        }

        // Mark session completed in state machine (before removal)
        // Already removed above, but we emit the event
        self.emit(BridgeEvent::SessionCompleted {
            session_id: session_id.to_string(),
        });

        // Free capacity
        self.poller.session_completed();

        // Archive metadata update
        let _ = &archive;

        Ok(())
    }

    /// Fail a session.
    pub fn fail_session(&mut self, session_id: &str, reason: String) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.state_machine.fail(reason.clone());
        }
        self.emit(BridgeEvent::SessionFailed {
            session_id: session_id.to_string(),
            reason,
        });
        self.poller.session_completed();
    }

    /// ─── Internal: Handle transport disconnect with reconnection ───
    #[allow(dead_code)]
    async fn handle_transport_disconnect(&mut self) {
        let attempt = self.reconnector.attempt_count() + 1;
        self.emit(BridgeEvent::TransportReconnecting { attempt });

        if let Some(delay) = self.reconnector.next_delay() {
            tokio::time::sleep(delay).await;
            // Attempt reconnect
            if let Some(transport) = self.transport.as_mut() {
                let _ = transport.connect().await;
                self.reconnector.reset();
                self.emit(BridgeEvent::TransportReconnected);
            }
        }
    }

    /// ─── Internal: Request permission for a tool call ───

    pub fn request_permission(&mut self, session_id: &str, tool: &str, args: &str) -> String {
        let request_id = self.permission_router.create_request(
            session_id.to_string(),
            tool.to_string(),
            args.to_string(),
        );
        self.emit(BridgeEvent::PermissionRequested {
            request_id: request_id.clone(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
        });
        request_id
    }

    /// ─── Internal: Check if a permission has been resolved ───

    #[must_use]
    pub fn is_permission_resolved(&self, request_id: &str) -> bool {
        self.permission_router.is_resolved(request_id)
    }

    /// ─── Internal: Get the permission response ───

    #[must_use]
    pub fn get_permission_response(&self, request_id: &str) -> Option<&PermissionResponse> {
        self.permission_router.get_response(request_id)
    }

    /// ─── Recovery state for a session ───

    #[must_use]
    pub fn create_recovery_state(&self, session_id: &str) -> Option<SessionRecoveryState> {
        self.sessions.get(session_id).map(|session| {
            SessionRecoveryState::new(
                session_id.to_string(),
                session.state_machine.current.label().to_string(),
            )
        })
    }

    /// Get a recovery state snapshot for all active sessions.
    #[must_use]
    pub fn all_recovery_states(&self) -> Vec<SessionRecoveryState> {
        self.sessions
            .iter()
            .filter(|(_, s)| !s.state_machine.is_terminal())
            .map(|(id, session)| {
                SessionRecoveryState::new(
                    id.clone(),
                    session.state_machine.current.label().to_string(),
                )
            })
            .collect()
    }
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            env_id: "default".into(),
            max_concurrent: 5,
            control_plane_url: "http://localhost:3000".into(),
            repo_root: PathBuf::from("."),
            worktree_base: PathBuf::from(".coderex/worktrees"),
            session_base: PathBuf::from(".coderex/sessions"),
            archive_base: PathBuf::from(".coderex/archives"),
            heartbeat: HeartbeatConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::InProcessTransport;

    fn test_config(base: &std::path::Path) -> BridgeConfig {
        BridgeConfig {
            env_id: "test_env".into(),
            max_concurrent: 3,
            control_plane_url: "http://localhost:3000".into(),
            repo_root: base.to_path_buf(),
            worktree_base: base.join("worktrees"),
            session_base: base.join("sessions"),
            archive_base: base.join("archives"),
            heartbeat: HeartbeatConfig::default(),
        }
    }

    fn test_bridge(base: &std::path::Path) -> Bridge {
        let config = test_config(base);
        let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
        Bridge::new(config, Box::new(a))
    }

    #[test]
    fn bridge_initial_state() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = test_bridge(tmp.path());
        assert!(!bridge.is_running());
        assert_eq!(bridge.active_session_count(), 0);
        assert_eq!(bridge.pending_assignment_count(), 0);
    }

    #[tokio::test]
    async fn bridge_connect_disconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        bridge.connect().await.unwrap();
        assert!(bridge.is_running());

        bridge.disconnect().await.unwrap();
        assert!(!bridge.is_running());
    }

    #[tokio::test]
    async fn bridge_connect_fails_without_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
        let mut bridge = Bridge::new(config, Box::new(a));

        // Connect should work (in-process doesn't need network)
        bridge.connect().await.unwrap();
        assert!(bridge.is_running());
    }

    #[test]
    fn bridge_event_subscription() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = test_bridge(tmp.path());
        let mut rx = bridge.subscribe();

        bridge.emit(BridgeEvent::SessionSpawned {
            session_id: "s1".into(),
            work_dir: "/tmp/s1".into(),
        });

        let event = rx.try_recv().unwrap();
        if let BridgeEvent::SessionSpawned { session_id, .. } = event {
            assert_eq!(session_id, "s1");
        } else {
            panic!("expected SessionSpawned event");
        }
    }

    #[test]
    fn bridge_permission_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        let id = bridge.request_permission("s1", "Bash", "ls -la");
        assert!(!bridge.is_permission_resolved(&id));
        assert_eq!(bridge.pending_permission_count("s1"), 1);

        bridge.respond_to_permission(&id, PermissionResponse::Allow { always: false });
        assert!(bridge.is_permission_resolved(&id));
    }

    #[test]
    fn bridge_active_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        bridge.receive_assignment(WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp/s1".into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::SingleSession,
        });

        assert_eq!(bridge.pending_assignment_count(), 1);
    }

    #[test]
    fn bridge_heartbeat_status() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = test_bridge(tmp.path());
        // Heartbeat starts healthy
        assert_eq!(bridge.heartbeat_status(), HeartbeatStatus::Healthy);
    }

    #[test]
    fn bridge_connection_id() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = test_bridge(tmp.path());
        assert_eq!(bridge.connection_id(), Some("bridge"));
    }

    #[tokio::test]
    async fn bridge_spawn_session_single() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        let config = test_config(tmp.path());
        let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
        let mut bridge = Bridge::new(config, Box::new(a));

        bridge.connect().await.unwrap();

        let assignment = WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: tmp.path().join("work").to_str().unwrap().into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::SingleSession,
        };

        let result = bridge.spawn_session(&assignment).await;
        assert!(result.is_ok());
        assert_eq!(result.as_ref().unwrap().session_id, "s1");
        assert!(!result.unwrap().worktree_created);

        // Session is tracked
        assert!(bridge.sessions.contains_key("s1"));
    }

    #[tokio::test]
    async fn bridge_complete_session_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        std::fs::create_dir_all(tmp.path().join("archives")).unwrap();
        let config = test_config(tmp.path());
        let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
        let mut bridge = Bridge::new(config, Box::new(a));

        bridge.connect().await.unwrap();

        // Spawn
        let assignment = WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: tmp.path().join("work").to_str().unwrap().into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::SingleSession,
        };
        bridge.spawn_session(&assignment).await.unwrap();
        assert!(bridge.sessions.contains_key("s1"));

        // Permission request
        let perm_id = bridge.request_permission("s1", "Read", "file.txt");
        assert!(bridge.pending_permission_count("s1") > 0);
        bridge.respond_to_permission(&perm_id, PermissionResponse::Allow { always: false });

        // Complete with archival
        let usage = UsageStats {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: 0.01,
        };
        bridge.complete_session("s1", usage).await.unwrap();

        // Session is no longer tracked
        assert!(!bridge.sessions.contains_key("s1"));
    }

    #[test]
    fn bridge_fail_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        bridge.receive_assignment(WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: "/tmp/s1".into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::SingleSession,
        });

        bridge.fail_session("s1", "spawn failed".into());

        // Session marked failed
        if let Some(session) = bridge.sessions.get("s1") {
            assert!(session.state_machine.is_terminal());
        }
    }

    #[test]
    fn bridge_config_default() {
        let config = BridgeConfig::default();
        assert_eq!(config.max_concurrent, 5);
        assert_eq!(config.env_id, "default");
    }

    #[test]
    fn bridge_recovery_states() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        // Manually insert a session
        bridge.sessions.insert(
            "s1".into(),
            ManagedSession {
                state_machine: BridgeSessionStateMachine::new("s1".into()),
                work_dir: PathBuf::from("/tmp/s1"),
                worktree_created: false,
            },
        );

        let states = bridge.all_recovery_states();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].session_id, "s1");

        let single = bridge.create_recovery_state("s1");
        assert!(single.is_some());
    }
}
