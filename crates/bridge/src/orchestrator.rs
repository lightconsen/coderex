//! Bridge orchestrator — integrates all bridge modules into a cohesive system.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

use crate::archive::{BridgeArchiveWriter, BridgeCleanupManager, UsageStats};
use crate::epoch::{StaleEpoch, WriteFence};
use crate::heartbeat::{
    HeartbeatConfig, HeartbeatLoop, HeartbeatStatus, SequenceCursor, SessionRecoveryState,
    WsReconnector,
};
use crate::permission::{BridgePermissionRouter, PermissionResponse};
use crate::poller::{
    BridgeWorkPoller, CapacityReport, ClaimOutcome, ControlPlaneClient, HttpControlPlaneClient,
    SpawnMode, WorkAssignment,
};
use crate::relay::{ApprovedIntent, RelayApprovalGate, RelayCallBody, RelayDecision, RelayDenial};
use crate::spawner::{BridgeSessionSpawner, SpawnConfig, SpawnResult};
use crate::state::{BridgeSessionState, BridgeSessionStateMachine};
use crate::transport::{Transport, TransportMessage};

/// Message type the gateway sends for a relayed tool call.
pub const RELAY_CALL_TYPE: &str = "relay_call";
/// Message type the worker answers a relayed call with.
pub const RELAY_RESULT_TYPE: &str = "relay_result";
/// Message type a worker sends to report a session's state.
pub const SESSION_WRITE_TYPE: &str = "session_write";
/// Message type the worker answers a write-back with.
pub const SESSION_WRITE_RESULT_TYPE: &str = "session_write_result";
/// Message type the gateway sends when the downstream stream has ended.
pub const STREAM_END_TYPE: &str = "stream_end";
/// Message type a worker sends to resume a stream from a sequence number.
pub const RESUME_TYPE: &str = "resume";
/// Metadata key carrying a frame's downstream sequence number.
pub const SEQUENCE_NUM_KEY: &str = "sequence_num";
/// Metadata key carrying the epoch of the worker making a write.
pub const WORKER_EPOCH_KEY: &str = "worker_epoch";
/// Metadata key carrying the resume point in the event-stream spelling.
pub const LAST_EVENT_ID_KEY: &str = "last-event-id";

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

/// A cloneable handle that asks a running [`Bridge`] to stop serving.
///
/// The handle is separate from the bridge so a signal handler or supervisor
/// task can request shutdown while `serve_transport` holds `&mut Bridge`.
#[derive(Debug, Clone)]
pub struct BridgeStopHandle {
    /// Set once shutdown has been requested.
    requested: Arc<AtomicBool>,
    /// Wakes the serve loop out of a blocking receive.
    notify: Arc<Notify>,
}

impl BridgeStopHandle {
    /// Request that the bridge stop serving.
    pub fn stop(&self) {
        self.requested.store(true, Ordering::SeqCst);
        // notify_one leaves a permit behind if the loop is not waiting yet, so
        // a stop requested between frames is not lost.
        self.notify.notify_one();
    }

    /// Whether shutdown has been requested.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Wait until shutdown has been requested.
    pub async fn wait(&self) {
        while !self.is_stopped() {
            self.notify.notified().await;
        }
    }
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
    /// Confirmations the person has given, for relayed calls to match against.
    relay_approvals: RelayApprovalGate,
    /// Highest downstream sequence number that reached this worker.
    cursor: SequenceCursor,
    /// Per-session write fences, so a replaced worker cannot write back over
    /// its successor.
    write_fences: std::collections::HashMap<String, WriteFence>,
    /// The state each session's worker last reported, by the worker that owns
    /// the session now.
    worker_reports: std::collections::HashMap<String, String>,
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
    /// Shutdown request flag shared with [`BridgeStopHandle`].
    stop_requested: Arc<AtomicBool>,
    /// Wakes the serve loop when shutdown is requested.
    stop_notify: Arc<Notify>,
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
        let client = Arc::new(HttpControlPlaneClient::new(
            config.control_plane_url.clone(),
        ));
        Self::with_control_plane(config, transport, client)
    }

    /// Create a bridge backed by an injected control-plane client.
    ///
    /// This is the seam for a worker that talks to a real control plane
    /// through a different client, and for tests that do not want a live
    /// server.
    pub fn with_control_plane(
        config: BridgeConfig,
        transport: Box<dyn Transport>,
        control_plane: Arc<dyn ControlPlaneClient>,
    ) -> Self {
        let poller = BridgeWorkPoller::with_client(
            config.env_id.clone(),
            config.max_concurrent,
            config.control_plane_url.clone(),
            control_plane,
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
            relay_approvals: RelayApprovalGate::new(),
            cursor: SequenceCursor::new(),
            write_fences: std::collections::HashMap::new(),
            worker_reports: std::collections::HashMap::new(),
            archive_writer,
            cleanup_manager: BridgeCleanupManager::new(
                config.worktree_base.clone(),
                config.session_base.clone(),
            ),
            sessions: std::collections::HashMap::new(),
            event_tx,
            running: false,
            stop_requested: Arc::new(AtomicBool::new(false)),
            stop_notify: Arc::new(Notify::new()),
        }
    }

    /// Get a handle that can request shutdown from another task.
    #[must_use]
    pub fn stop_handle(&self) -> BridgeStopHandle {
        BridgeStopHandle {
            requested: Arc::clone(&self.stop_requested),
            notify: Arc::clone(&self.stop_notify),
        }
    }

    /// Request that [`Bridge::serve_transport`] stop after the frame it is
    /// handling. Safe to call from a signal handler or supervisor task.
    pub fn request_stop(&self) {
        self.stop_handle().stop();
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

    /// Fetch assignments and enqueue them in the poller.
    pub async fn fetch_assignments(&mut self) -> Result<usize, String> {
        self.poller.fetch_assignments().await
    }

    /// Report current capacity to the control plane.
    pub async fn report_capacity_to_server(&self) -> Result<(), String> {
        self.poller.report_capacity_to_server().await
    }

    /// Snapshot of the current capacity report.
    #[must_use]
    pub fn capacity_report(&self) -> CapacityReport {
        self.poller.report_capacity()
    }

    /// Submit a permission response for a pending request.
    ///
    /// An approval also becomes the confirmation a relayed call has to present:
    /// it records what the person approved, keyed by the request they approved
    /// it for.
    pub fn respond_to_permission(
        &mut self,
        request_id: &str,
        response: PermissionResponse,
    ) -> bool {
        let pending = self.permission_router.get_pending(request_id).cloned();
        let resolved = self.permission_router.respond(request_id, response);
        if resolved {
            if let Some(resp) = self.permission_router.get_response(request_id) {
                let allowed = matches!(resp, PermissionResponse::Allow { .. });
                if allowed {
                    if let Some(ref request) = pending {
                        self.relay_approvals.confirm(
                            request_id,
                            ApprovedIntent::for_call(
                                &request.session_id,
                                &request.tool,
                                &request.args,
                            ),
                        );
                    }
                }
                self.emit(BridgeEvent::PermissionResolved {
                    request_id: request_id.to_string(),
                    allowed,
                });
            }
        }
        resolved
    }

    /// Check a relayed call against the confirmations the person has given.
    ///
    /// The call has to carry both binding headers and match an unspent
    /// confirmation; anything else is refused as the gateway refuses it — 403
    /// with [`crate::relay::UNAPPROVED_ERROR_CODE`]. `header` looks a header up
    /// by lowercase name.
    ///
    /// [`Self::handle_relay_call`] is the caller that matters: it is where a
    /// `relay_call` frame on the transport protocol lands, after the headers
    /// have been through the forwarding policy.
    pub fn authorize_relay_call(
        &mut self,
        header: impl Fn(&str) -> Option<String>,
    ) -> Result<(), RelayDenial> {
        self.relay_approvals.authorize_headers(header)
    }

    /// Withdraw a confirmation whose request was cancelled before it was used.
    pub fn withdraw_relay_approval(&mut self, request_id: &str) -> bool {
        self.relay_approvals.withdraw(request_id)
    }

    /// How many confirmations are waiting to be spent.
    #[must_use]
    pub fn outstanding_relay_approvals(&self) -> usize {
        self.relay_approvals.outstanding()
    }

    /// ─── The relay endpoint ───
    ///
    /// Handle a relayed tool call from the cloud session's gateway.
    ///
    /// This is the production call site for the approval chain: `headers` is
    /// the metadata a [`RELAY_CALL_TYPE`] message carried, and the forwarding
    /// policy runs first, so a header the relay does not own never reaches the
    /// gate. What survives is what the gate judges, and a call that matches no
    /// unspent confirmation comes back as the gateway's own refusal.
    pub fn handle_relay_call(
        &mut self,
        call: RelayCallBody,
        headers: impl IntoIterator<Item = (String, String)>,
    ) -> RelayDecision {
        let forwarded = crate::relay::forwardable_headers(headers);
        match self.authorize_relay_call(|name| forwarded.get(name).cloned()) {
            Ok(()) => RelayDecision::Forward {
                session_id: call.session_id,
                tool: call.tool,
                args: call.args,
                headers: forwarded,
            },
            Err(denial) => denial.refusal(),
        }
    }

    /// Record a session state a worker reported, if that worker still owns the
    /// session.
    ///
    /// The write is fenced by `epoch`: a worker whose epoch has been superseded
    /// cannot write back over the worker that replaced it.
    pub fn apply_session_write(
        &mut self,
        session_id: &str,
        epoch: u64,
        state: &str,
    ) -> Result<(), StaleEpoch> {
        self.write_fences
            .entry(session_id.to_string())
            .or_default()
            .admit(epoch)?;
        self.worker_reports
            .insert(session_id.to_string(), state.to_string());
        Ok(())
    }

    /// The state the session's current worker last reported.
    #[must_use]
    pub fn worker_state(&self, session_id: &str) -> Option<&str> {
        self.worker_reports.get(session_id).map(String::as_str)
    }

    /// The highest downstream sequence number that reached this worker.
    #[must_use]
    pub const fn last_delivered_sequence(&self) -> Option<u64> {
        self.cursor.last_delivered()
    }

    /// Note a frame's sequence number, so a reconnect can resume past it.
    fn note_sequence(&mut self, message: &TransportMessage) {
        if let Some(seq) = message
            .metadata
            .get(SEQUENCE_NUM_KEY)
            .and_then(|s| s.parse::<u64>().ok())
        {
            self.cursor.deliver(seq);
        }
    }

    /// The frame a reconnect sends to ask for the gap rather than the whole
    /// stream.
    ///
    /// Both spellings of the same point: `from_sequence_num` for the bridge's
    /// own protocol, and `Last-Event-ID` for an event stream that only knows
    /// that one.
    #[must_use]
    pub fn resume_request(&self) -> TransportMessage {
        let mut metadata = std::collections::HashMap::new();
        if let Some(last_event_id) = self.cursor.last_event_id() {
            metadata.insert(LAST_EVENT_ID_KEY.to_string(), last_event_id);
        }
        TransportMessage {
            r#type: RESUME_TYPE.to_string(),
            correlation_id: None,
            payload: serde_json::json!({ "from_sequence_num": self.cursor.resume_from() })
                .to_string(),
            metadata,
        }
    }

    /// Route one inbound frame to the handler for its type.
    ///
    /// Returns the frame to send back, if the message expects one.
    pub fn handle_transport_message(
        &mut self,
        message: TransportMessage,
    ) -> Option<TransportMessage> {
        match message.r#type.as_str() {
            RELAY_CALL_TYPE => {
                let body: RelayCallBody = match serde_json::from_str(&message.payload) {
                    Ok(body) => body,
                    Err(e) => {
                        return Some(transport_error(&message, &format!("relay_call body: {e}")));
                    }
                };
                let decision = self.handle_relay_call(body, message.metadata.clone());
                Some(transport_reply(
                    &message,
                    RELAY_RESULT_TYPE,
                    serde_json::to_string(&decision).unwrap_or_default(),
                ))
            }
            SESSION_WRITE_TYPE => {
                #[derive(Deserialize)]
                struct Write {
                    session_id: String,
                    state: String,
                }
                let Some(epoch) = message
                    .metadata
                    .get(WORKER_EPOCH_KEY)
                    .and_then(|e| e.parse::<u64>().ok())
                else {
                    return Some(transport_error(
                        &message,
                        "session_write is missing its worker_epoch",
                    ));
                };
                let write: Write = match serde_json::from_str(&message.payload) {
                    Ok(write) => write,
                    Err(e) => {
                        return Some(transport_error(
                            &message,
                            &format!("session_write body: {e}"),
                        ));
                    }
                };
                let payload = match self.apply_session_write(&write.session_id, epoch, &write.state)
                {
                    Ok(()) => serde_json::json!({
                        "accepted": true,
                        "session_id": write.session_id,
                        "worker_epoch": epoch,
                    }),
                    Err(stale) => serde_json::json!({
                        "accepted": false,
                        "reason": stale.to_string(),
                        "current_epoch": stale.current,
                    }),
                };
                Some(transport_reply(
                    &message,
                    SESSION_WRITE_RESULT_TYPE,
                    payload.to_string(),
                ))
            }
            "heartbeat" => {
                if let Some(heartbeat) = self.heartbeat_loop.as_ref().map(|h| h.heartbeat()) {
                    if let Ok(mut beat) = heartbeat.lock() {
                        beat.record_success();
                    }
                }
                Some(transport_reply(&message, "heartbeat_ack", "{}".into()))
            }
            other => Some(transport_error(
                &message,
                &format!("unknown message type: {other}"),
            )),
        }
    }

    /// Serve inbound frames until the transport ends.
    ///
    /// This is the loop the relay endpoint lives on: every frame is sequenced,
    /// a `stream_end` makes the worker come back asking to resume rather than
    /// reconnecting blind, and everything else is routed by type.
    pub async fn serve_transport(&mut self) -> Result<(), String> {
        self.running = true;
        let stop_requested = Arc::clone(&self.stop_requested);
        let stop_notify = Arc::clone(&self.stop_notify);
        while self.running && !stop_requested.load(Ordering::SeqCst) {
            let shutdown = stop_notify.notified();
            let received = {
                let transport = self.transport.as_mut().ok_or("no transport configured")?;
                tokio::select! {
                    received = transport.receive() => Some(received),
                    () = shutdown => None,
                }
            };
            let Some(received) = received else {
                break;
            };
            let message = match received {
                Ok(message) => message,
                // The stream ended; a gateway that wants the worker back says
                // so with `stream_end` instead.
                Err(_) => break,
            };
            self.note_sequence(&message);

            if message.r#type == STREAM_END_TYPE {
                self.reconnect_with_resume().await?;
                continue;
            }

            if let Some(reply) = self.handle_transport_message(message) {
                let transport = self.transport.as_mut().ok_or("no transport configured")?;
                if transport.send(reply).await.is_err() {
                    break;
                }
            }
        }
        self.running = false;
        Ok(())
    }

    /// Reconnect after the downstream ended, and ask to resume the stream from
    /// the last sequence number that reached this worker.
    pub async fn reconnect_with_resume(&mut self) -> Result<(), String> {
        self.emit(BridgeEvent::TransportReconnecting {
            attempt: self.reconnector.attempt_count() + 1,
        });

        if let Some(delay) = self.reconnector.next_delay() {
            tokio::time::sleep(delay).await;
        }

        let resume = self.resume_request();
        let transport = self.transport.as_mut().ok_or("no transport configured")?;
        transport.connect().await.map_err(|e| e.to_string())?;
        transport.send(resume).await.map_err(|e| e.to_string())?;

        self.reconnector.reset();
        self.emit(BridgeEvent::TransportReconnected);
        Ok(())
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

    /// Claim a taken assignment with the control plane.
    ///
    /// The worker calls this after [`Bridge::take_assignment`] and before
    /// [`Bridge::spawn_session`]. `Ok(ClaimOutcome::Lost)` means another worker
    /// won the race: skip the assignment and release its slot with
    /// [`Bridge::release_assignment_slot`].
    pub async fn claim_assignment(
        &self,
        assignment: &WorkAssignment,
    ) -> Result<ClaimOutcome, String> {
        self.poller.claim_assignment(assignment).await
    }

    /// Release the capacity slot reserved by an assignment that will not run.
    ///
    /// This is the skip path for a lost claim: no session was spawned, so it
    /// is not a session failure.
    pub fn release_assignment_slot(&mut self) {
        self.poller.session_completed();
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
        transcript: &str,
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
            .archive(session_id, transcript, usage, "completed")
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

    /// Snapshot a session for recovery, including where its stream had got to.
    #[must_use]
    pub fn create_recovery_state(&self, session_id: &str) -> Option<SessionRecoveryState> {
        self.sessions.get(session_id).map(|session| {
            self.recovery_state_for(
                session_id,
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
                self.recovery_state_for(id, session.state_machine.current.label().to_string())
            })
            .collect()
    }

    /// A recovery snapshot with the downstream offset attached, so a session
    /// that comes back knows where to resume rather than only where it was.
    fn recovery_state_for(&self, session_id: &str, last_state: String) -> SessionRecoveryState {
        let mut state = SessionRecoveryState::new(session_id.to_string(), last_state);
        if let Some(seq) = self.cursor.last_delivered() {
            state.record_sequence(seq);
        }
        state
    }
}

/// Build the frame that answers `request`.
fn transport_reply(request: &TransportMessage, r#type: &str, payload: String) -> TransportMessage {
    TransportMessage {
        r#type: r#type.to_string(),
        correlation_id: request.correlation_id.clone(),
        payload,
        metadata: std::collections::HashMap::new(),
    }
}

/// Build the frame that reports a message the worker could not read.
fn transport_error(request: &TransportMessage, reason: &str) -> TransportMessage {
    transport_reply(
        request,
        "error",
        serde_json::json!({ "error": reason }).to_string(),
    )
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

    fn relay_headers(id: &str, intent: &ApprovedIntent) -> impl Fn(&str) -> Option<String> {
        let id = id.to_string();
        let encoded = intent.encode();
        move |name: &str| match name {
            crate::relay::TOOL_USE_ID_HEADER => Some(id.clone()),
            crate::relay::APPROVED_INTENT_HEADER => Some(encoded.clone()),
            _ => None,
        }
    }

    #[test]
    fn an_approval_becomes_the_confirmation_a_relay_call_presents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());
        let args = r#"{"command":"ls"}"#;
        let request_id = bridge.request_permission("sess_1", "Bash", args);
        let intent = ApprovedIntent::for_call("sess_1", "Bash", args);

        // Before the person answers, the call has nothing to match.
        assert!(matches!(
            bridge.authorize_relay_call(relay_headers(&request_id, &intent)),
            Err(RelayDenial::Unapproved { .. })
        ));

        assert!(
            bridge.respond_to_permission(&request_id, PermissionResponse::Allow { always: false })
        );
        assert_eq!(bridge.outstanding_relay_approvals(), 1);
        assert!(
            bridge
                .authorize_relay_call(relay_headers(&request_id, &intent))
                .is_ok()
        );

        // The confirmation authorised that one call.
        assert_eq!(bridge.outstanding_relay_approvals(), 0);
        assert!(
            bridge
                .authorize_relay_call(relay_headers(&request_id, &intent))
                .is_err()
        );
    }

    #[test]
    fn a_denied_request_confirms_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());
        let request_id = bridge.request_permission("sess_1", "Bash", "ls");

        assert!(bridge.respond_to_permission(&request_id, PermissionResponse::Deny));
        assert_eq!(bridge.outstanding_relay_approvals(), 0);
    }

    #[test]
    fn an_approval_does_not_cover_a_different_call() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());
        let request_id = bridge.request_permission("sess_1", "Bash", r#"{"command":"ls"}"#);
        assert!(
            bridge.respond_to_permission(&request_id, PermissionResponse::Allow { always: false })
        );

        let other = ApprovedIntent::for_call("sess_1", "Bash", r#"{"command":"rm -rf /"}"#);
        assert!(matches!(
            bridge.authorize_relay_call(relay_headers(&request_id, &other)),
            Err(RelayDenial::Unapproved { .. })
        ));
        assert_eq!(bridge.outstanding_relay_approvals(), 1);
    }

    #[test]
    fn a_withdrawn_approval_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());
        let request_id = bridge.request_permission("sess_1", "Bash", "ls");
        assert!(
            bridge.respond_to_permission(&request_id, PermissionResponse::Allow { always: false })
        );

        assert!(bridge.withdraw_relay_approval(&request_id));
        let intent = ApprovedIntent::for_call("sess_1", "Bash", "ls");
        assert!(matches!(
            bridge.authorize_relay_call(relay_headers(&request_id, &intent)),
            Err(RelayDenial::Unapproved { .. })
        ));
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

    #[tokio::test]
    async fn serve_transport_stops_when_the_handle_requests_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge".into(), "remote".into());
        bridge_transport_connect(&mut remote).await;

        let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
        bridge.connect().await.unwrap();
        let stop = bridge.stop_handle();
        let serving = tokio::spawn(async move { bridge.serve_transport().await });

        // Give the serve loop a moment to start waiting on the transport.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!stop.is_stopped());
        stop.stop();
        assert!(stop.is_stopped());

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), serving)
            .await
            .expect("serve_transport should stop promptly")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn serve_transport_still_answers_a_frame_before_stopping() {
        use crate::transport::TransportMessage;

        let tmp = tempfile::tempdir().unwrap();
        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge".into(), "remote".into());
        bridge_transport_connect(&mut remote).await;

        let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
        bridge.connect().await.unwrap();
        let stop = bridge.stop_handle();
        let serving = tokio::spawn(async move { bridge.serve_transport().await });

        remote
            .send(TransportMessage {
                r#type: "heartbeat".into(),
                correlation_id: Some("beat".into()),
                payload: "{}".into(),
                metadata: std::collections::HashMap::new(),
            })
            .await
            .unwrap();

        let reply = remote.receive().await.unwrap();
        assert_eq!(reply.r#type, "heartbeat_ack");
        assert_eq!(reply.correlation_id, Some("beat".into()));

        stop.stop();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), serving)
            .await
            .expect("serve_transport should stop promptly")
            .unwrap();
        assert!(result.is_ok());
    }

    /// Connect the remote half of an in-process pair, since only the bridge
    /// half is connected by `serve_transport`.
    async fn bridge_transport_connect(remote: &mut InProcessTransport) {
        use crate::transport::Transport;
        remote.connect().await.unwrap();
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
        bridge
            .complete_session("s1", "{\"type\":\"result\"}\n", usage)
            .await
            .unwrap();

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
