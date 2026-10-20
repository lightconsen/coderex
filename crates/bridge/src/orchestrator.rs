//! Bridge orchestrator — integrates all bridge modules into a cohesive system.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

use crate::archive::{BridgeArchiveWriter, BridgeCleanupManager, UsageStats};
use crate::epoch::{StaleEpoch, WriteFence};
use crate::gateway::PermissionFrameRelay;
use crate::heartbeat::{
    HeartbeatConfig, HeartbeatLoop, HeartbeatStatus, SequenceCursor, SessionRecoveryState,
    WsReconnector,
};
use crate::permission::{BridgePermissionRouter, PendingPermission, PermissionResponse};
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
/// Message type the bridge sends when a session needs a person's approval.
pub const PERMISSION_REQUEST_TYPE: &str = "permission_request";
/// Message type the gateway (or a client behind it) sends to answer one.
pub const PERMISSION_RESPONSE_TYPE: &str = "permission_response";
/// Message type the bridge answers a permission response with.
pub const PERMISSION_RESULT_TYPE: &str = "permission_result";
/// Message type carrying one line of a session's stdout or stderr.
pub const SESSION_OUTPUT_TYPE: &str = "session_output";
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
    /// Whether a reconnect is owed to the transport.
    ///
    /// The launcher recreates `serve_transport`'s future whenever another
    /// branch of its select wins (a poll tick, a finished session), so the
    /// state that says "we were dropped and have not reconnected yet" has to
    /// outlive one call.
    reconnect_pending: bool,
    /// When the current backoff wait ends. Kept across calls for the same
    /// reason: a cancelled sleep must resume, not restart from the top.
    reconnect_deadline: Option<tokio::time::Instant>,
    /// Why the most recent reconnect attempt failed, for the terminal error.
    reconnect_last_error: String,
    /// Work poller for receiving assignments.
    poller: BridgeWorkPoller,
    /// Session spawner for worktree/directory creation.
    spawner: BridgeSessionSpawner,
    /// Permission router for tool call authorization.
    permission_router: BridgePermissionRouter,
    /// Gateway-side relay for `permission_request`/`permission_response`.
    ///
    /// The bridge drives this on its own transport: an outbound request is
    /// relayed before it goes on the wire, and an inbound response is relayed
    /// before it resolves anything. The pending set is what makes an answer for
    /// an id the worker never asked about — or a replay of one already
    /// answered — refused instead of delivered twice.
    permission_relay: PermissionFrameRelay,
    /// Confirmations the person has given, for relayed calls to match against.
    relay_approvals: RelayApprovalGate,
    /// Permission requests waiting to be sent to the gateway.
    ///
    /// `request_permission` can be called by any bridge-side producer; the
    /// transport is owned by the serve loop, so the frames wait here until
    /// [`Bridge::send_pending_permission_requests`] flushes them.
    pending_permission_requests: Vec<TransportMessage>,
    /// Fenced state writes waiting to be sent to the gateway.
    ///
    /// Like permission requests, these are queued while a producer owns
    /// `&mut Bridge` and flushed by [`Bridge::send_pending_session_writes`].
    pending_session_writes: Vec<TransportMessage>,
    /// Counter behind each session write's correlation id.
    next_session_write_id: u64,
    /// Caller-supplied request ids mapped to the router's internal ids.
    ///
    /// A child process generates its own request ids. The router generates
    /// `perm_N`, so a request that arrives with an id already on the wire
    /// keeps that id externally and resolves through this alias.
    permission_aliases: std::collections::HashMap<String, String>,
    /// Every caller-supplied id ever registered, including answered ones.
    ///
    /// A repeated id from the child is a protocol error, not a new question;
    /// this is what makes that detectable after the alias is removed.
    known_permission_ids: std::collections::HashSet<String>,
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
            reconnect_pending: false,
            reconnect_deadline: None,
            reconnect_last_error: String::new(),
            poller,
            spawner,
            permission_router: BridgePermissionRouter::new(),
            permission_relay: PermissionFrameRelay::new(),
            relay_approvals: RelayApprovalGate::new(),
            pending_permission_requests: Vec::new(),
            pending_session_writes: Vec::new(),
            next_session_write_id: 0,
            permission_aliases: std::collections::HashMap::new(),
            known_permission_ids: std::collections::HashSet::new(),
            cursor: SequenceCursor::new(),
            write_fences: std::collections::HashMap::new(),
            worker_reports: std::collections::HashMap::new(),
            archive_writer,
            cleanup_manager: BridgeCleanupManager::new(
                config.worktree_base.clone(),
                config.session_base,
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
    pub const fn is_running(&self) -> bool {
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

    /// The worker epoch registration assigned, or `0` before registration (or
    /// when the control plane did not report one).
    ///
    /// A zero epoch means write-backs are disabled: the gateway could not
    /// order this worker against the one it replaced.
    #[must_use]
    pub fn worker_epoch(&self) -> u64 {
        self.poller.worker_epoch()
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
        // A child-originated request reached the gateway under the child's
        // own id; the router knows it under the id generated for it.
        let router_id = self
            .permission_aliases
            .get(request_id)
            .cloned()
            .unwrap_or_else(|| request_id.to_string());
        let pending = self.permission_router.get_pending(&router_id).cloned();
        let resolved = self.permission_router.respond(&router_id, response);
        if resolved {
            // First answer wins: the relay must stop expecting this id, so a
            // second `permission_response` for it is refused rather than
            // forwarded. This is a no-op when the answer arrived through
            // `handle_transport_message`, which already removed the entry.
            self.permission_relay.forget(request_id);
            if let Some(resp) = self.permission_router.get_response(&router_id) {
                let allowed = matches!(resp, PermissionResponse::Allow { .. });
                if allowed {
                    if let Some(ref request) = pending {
                        // Keyed by the id the person actually saw and the
                        // relayed call will carry.
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
            // The alias stays: the response has to remain readable under the
            // id the gateway knows, and `known_permission_ids` is what
            // rejects a reused id.
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
        message: &TransportMessage,
    ) -> Option<TransportMessage> {
        match message.r#type.as_str() {
            RELAY_CALL_TYPE => {
                let body: RelayCallBody = match serde_json::from_str(&message.payload) {
                    Ok(body) => body,
                    Err(e) => {
                        return Some(transport_error(message, &format!("relay_call body: {e}")));
                    }
                };
                let decision = self.handle_relay_call(body, message.metadata.clone());
                Some(transport_reply(
                    message,
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
                        message,
                        "session_write is missing its worker_epoch",
                    ));
                };
                let write: Write = match serde_json::from_str(&message.payload) {
                    Ok(write) => write,
                    Err(e) => {
                        return Some(transport_error(
                            message,
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
                    message,
                    SESSION_WRITE_RESULT_TYPE,
                    payload.to_string(),
                ))
            }
            // `approval` is accepted as an alias so a gateway that spells the
            // answer that way still reaches the same router and gate.
            PERMISSION_RESPONSE_TYPE | "approval" => {
                let response: PermissionResponseFrame = match serde_json::from_str(&message.payload)
                {
                    Ok(response) => response,
                    Err(e) => {
                        return Some(transport_error(
                            message,
                            &format!("permission_response body: {e}"),
                        ));
                    }
                };
                let (request_id, response) = response.into_response();
                // Route the answer through the relay before it resolves
                // anything: only a response for an outstanding request is
                // forwarded. An answer for an id the worker never asked about,
                // or a replay of one already answered, is refused here so the
                // child is never handed a second decision.
                if self
                    .permission_relay
                    .relay_host_frame(message.clone())
                    .is_none()
                {
                    return Some(transport_reply(
                        message,
                        PERMISSION_RESULT_TYPE,
                        serde_json::json!({
                            "request_id": request_id,
                            "resolved": false,
                        })
                        .to_string(),
                    ));
                }
                let resolved = self.respond_to_permission(&request_id, response);
                Some(transport_reply(
                    message,
                    PERMISSION_RESULT_TYPE,
                    serde_json::json!({
                        "request_id": request_id,
                        "resolved": resolved,
                    })
                    .to_string(),
                ))
            }
            "heartbeat" => {
                if let Some(heartbeat) = self
                    .heartbeat_loop
                    .as_ref()
                    .map(super::heartbeat::HeartbeatLoop::heartbeat)
                {
                    if let Ok(mut beat) = heartbeat.lock() {
                        beat.record_success();
                    }
                }
                Some(transport_reply(message, "heartbeat_ack", "{}".into()))
            }
            other => Some(transport_error(
                message,
                &format!("unknown message type: {other}"),
            )),
        }
    }

    /// Serve inbound frames until the transport ends.
    ///
    /// This is the loop the relay endpoint lives on: every frame is sequenced,
    /// a `stream_end` makes the worker come back asking to resume rather than
    /// reconnecting blind, and everything else is routed by type. A dropped
    /// socket is retried with bounded backoff and the same resume cursor; once
    /// the attempts are exhausted the loop returns an error, which the launcher
    /// treats as terminal.
    pub async fn serve_transport(&mut self) -> Result<(), String> {
        self.running = true;
        let stop_requested = Arc::clone(&self.stop_requested);
        let stop_notify = Arc::clone(&self.stop_notify);
        while self.running && !stop_requested.load(Ordering::SeqCst) {
            // This future is recreated whenever the launcher's select picks
            // another branch, so a reconnect that was interrupted mid-backoff
            // is resumed here instead of being lost.
            if self.reconnect_pending {
                self.reconnect_with_resume().await?;
                continue;
            }

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
                // A dropped socket is not the end of the worker: reconnect
                // with the resume cursor, exactly as a `stream_end` does.
                // When the bounded attempts run out, surface it as terminal.
                Err(err) => {
                    if stop_requested.load(Ordering::SeqCst) {
                        break;
                    }
                    tracing::warn!(error = %err, "bridge transport dropped; reconnecting");
                    self.reconnect_pending = true;
                    continue;
                }
            };
            self.note_sequence(&message);

            if message.r#type == STREAM_END_TYPE {
                self.reconnect_pending = true;
                continue;
            }

            if let Some(reply) = self.handle_transport_message(&message) {
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
    ///
    /// Retries with the reconnector's bounded exponential backoff. `Err` means
    /// the attempts are exhausted (or no transport is configured), which the
    /// serve loop treats as terminal. A shutdown requested while waiting for a
    /// delay stops the retry loop without error so the serve loop can exit
    /// promptly.
    pub async fn reconnect_with_resume(&mut self) -> Result<(), String> {
        self.reconnect_pending = true;

        loop {
            // Pick the deadline once and keep it in the bridge: this future is
            // cancelled whenever the launcher's select takes another branch,
            // and restarting the backoff from zero would let a steady tick or
            // output stream postpone reconnection indefinitely.
            let deadline = if let Some(deadline) = self.reconnect_deadline {
                deadline
            } else {
                let Some(delay) = self.reconnector.next_delay() else {
                    self.reconnect_pending = false;
                    return Err(format!(
                        "transport reconnect exhausted after {} attempts: {}",
                        self.reconnector.attempt_count(),
                        self.reconnect_last_error,
                    ));
                };

                self.emit(BridgeEvent::TransportReconnecting {
                    attempt: self.reconnector.attempt_count(),
                });

                let deadline = tokio::time::Instant::now() + delay;
                self.reconnect_deadline = Some(deadline);
                deadline
            };

            let stop_notify = Arc::clone(&self.stop_notify);
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {}
                () = stop_notify.notified() => {
                    if self.stop_requested.load(Ordering::SeqCst) {
                        self.reconnect_pending = false;
                        self.reconnect_deadline = None;
                        return Ok(());
                    }
                }
            }

            // The wait is over; this attempt is spent even if it fails.
            self.reconnect_deadline = None;

            let resume = self.resume_request();
            let transport = self.transport.as_mut().ok_or("no transport configured")?;
            match transport.connect().await {
                Ok(()) => match transport.send(resume).await {
                    Ok(()) => {
                        self.reconnector.reset();
                        self.reconnect_pending = false;
                        self.emit(BridgeEvent::TransportReconnected);
                        return Ok(());
                    }
                    Err(err) => self.reconnect_last_error = format!("resume send failed: {err}"),
                },
                Err(err) => self.reconnect_last_error = format!("reconnect failed: {err}"),
            }
        }
    }

    /// Get the heartbeat status.
    #[must_use]
    pub fn heartbeat_status(&self) -> HeartbeatStatus {
        self.heartbeat_loop
            .as_ref()
            .map_or(HeartbeatStatus::Lost, |hb| {
                let state = hb.heartbeat();
                let guard = state.lock().unwrap();
                guard.status()
            })
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
    pub const fn release_assignment_slot(&mut self) {
        self.poller.session_completed();
    }

    /// Get the permission router's pending count for a session.
    #[must_use]
    pub fn pending_permission_count(&self, session_id: &str) -> usize {
        self.permission_router.pending_for_session(session_id).len()
    }

    /// How many permission requests the relay is still waiting to see answered.
    ///
    /// This is the set an inbound `permission_response` is checked against, so
    /// it is what makes an unknown or duplicate answer refusable.
    #[must_use]
    pub fn outstanding_relayed_permissions(&self) -> usize {
        self.permission_relay.pending()
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
                _ => None,
            },
        };

        // Drive the state machine up to the point where the process can be
        // started. `CreatingWorktree` is entered before the spawner runs only
        // when that mode actually creates one; the session is registered once
        // the spawn succeeded, keeping its full transition history.
        let mut state_machine = BridgeSessionStateMachine::new(session_id.clone());
        state_machine
            .transition(BridgeSessionState::Preparing)
            .map_err(|e| format!("state transition failed: {e}"))?;
        let creates_worktree = matches!(assignment.spawn_mode, SpawnMode::Worktree);
        if creates_worktree {
            state_machine
                .transition(BridgeSessionState::CreatingWorktree)
                .map_err(|e| format!("state transition failed: {e}"))?;
        }

        let result = self.spawner.spawn(config).await?;

        state_machine
            .transition(BridgeSessionState::Spawning)
            .map_err(|e| format!("state transition failed: {e}"))?;

        self.sessions.insert(
            session_id.clone(),
            ManagedSession {
                state_machine,
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
        // Drive the session to Running through the validated graph, then
        // archive, clean up, and finish at Completed.
        if let Some(session) = self.sessions.get_mut(session_id) {
            drive_to_running(&mut session.state_machine, session.worktree_created)?;
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

        // The machine requires CleaningUp → Completed before the session is
        // dropped from tracking.
        if let Some(session) = self.sessions.get_mut(session_id) {
            session
                .state_machine
                .transition(BridgeSessionState::Completed)
                .map_err(|e| format!("state transition failed: {e}"))?;
        }

        // Clean up worktree if created
        if let Some(session) = self.sessions.remove(session_id) {
            if session.worktree_created {
                let _ = self.spawner.cleanup_worktree(session_id);
            }
        }

        // The session was transitioned through Completed above.
        self.emit(BridgeEvent::SessionCompleted {
            session_id: session_id.to_string(),
        });

        // Free capacity
        self.poller.session_completed();

        // Archive metadata update
        let _ = &archive;

        Ok(())
    }

    /// Fail a session through the validated transition graph.
    ///
    /// Every non-terminal state has an edge to `Failed`; a session already in
    /// a terminal state keeps the state it reached first.
    pub fn fail_session(&mut self, session_id: &str, reason: String) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if !session.state_machine.is_terminal() {
                let _ = session
                    .state_machine
                    .transition(BridgeSessionState::Failed(reason.clone()));
            }
        }
        self.emit(BridgeEvent::SessionFailed {
            session_id: session_id.to_string(),
            reason,
        });
        self.poller.session_completed();
    }

    /// Record that the spawned child is now the running session.
    ///
    /// The launcher calls this once the spawner has prepared the session and
    /// the runner is about to start, completing the `Spawning → Running` edge.
    pub fn mark_session_running(&mut self, session_id: &str) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("unknown session '{session_id}'"))?;
        if matches!(session.state_machine.current, BridgeSessionState::Running) {
            return Ok(());
        }
        session
            .state_machine
            .transition(BridgeSessionState::Running)
            .map_err(|e| format!("state transition failed: {e}"))
    }

    /// Enter `AwaitingPermission` because a child request is pending.
    ///
    /// Several requests can be outstanding at once; the state only changes on
    /// the first one, and [`Self::end_awaiting_permission`] is what leaves it.
    pub fn begin_awaiting_permission(&mut self, session_id: &str) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("unknown session '{session_id}'"))?;
        if matches!(
            session.state_machine.current,
            BridgeSessionState::AwaitingPermission
        ) {
            return Ok(());
        }
        session
            .state_machine
            .transition(BridgeSessionState::AwaitingPermission)
            .map_err(|e| format!("state transition failed: {e}"))
    }

    /// Return to `Running` once no child permission requests remain pending.
    ///
    /// A session that already failed is left alone: the answer arriving after
    /// the child died must not resurrect it.
    pub fn end_awaiting_permission(&mut self, session_id: &str) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("unknown session '{session_id}'"))?;
        if session.state_machine.is_terminal()
            || !matches!(
                session.state_machine.current,
                BridgeSessionState::AwaitingPermission
            )
        {
            return Ok(());
        }
        session
            .state_machine
            .transition(BridgeSessionState::Running)
            .map_err(|e| format!("state transition failed: {e}"))
    }

    /// Resolve a permission request as denied because its approval wait
    /// elapsed.
    ///
    /// Returns `false` when an answer already resolved the request (the
    /// bounded wait and a gateway response raced). Once this returns `true`
    /// the router's first-answer-wins rule makes every later response for the
    /// same id a no-op, so a late gateway answer cannot revive a dead request.
    pub fn expire_permission(&mut self, request_id: &str) -> bool {
        let router_id = self
            .permission_aliases
            .get(request_id)
            .cloned()
            .unwrap_or_else(|| request_id.to_string());

        // The worker has stopped waiting for this id either way, so the relay
        // must stop expecting it: a gateway answer that arrives after the
        // deadline is refused instead of relayed.
        self.permission_relay.forget(request_id);

        if self.permission_router.is_resolved(&router_id) {
            return false;
        }

        let resolved = self
            .permission_router
            .respond(&router_id, PermissionResponse::Deny);
        if resolved {
            self.emit(BridgeEvent::PermissionResolved {
                request_id: request_id.to_string(),
                allowed: false,
            });
        }
        resolved
    }

    // ─── Internal: Request permission for a tool call ───

    /// Create a pending permission request and queue the frame that tells the
    /// gateway about it.
    ///
    /// The production caller is `bridge_launcher`: the child is started
    /// headless with `stream-json` and a positional prompt, `read_stdout`
    /// turns its `control_request` frame into this call, and an inbound
    /// `permission_response` is written back to the child's stdin. The frame
    /// travels to the gateway through [`Self::send_pending_permission_requests`],
    /// which relays it through [`crate::gateway::PermissionFrameRelay`] before
    /// it goes on the wire; the answer comes back through
    /// [`Self::handle_transport_message`], which relays it the other way. The
    /// gateway service that owns the *host* connection supplies the rest:
    /// authentication, fan-out, and the human UI.
    pub fn request_permission(&mut self, session_id: &str, tool: &str, args: &str) -> String {
        let request_id = self.permission_router.create_request(
            session_id.to_string(),
            tool.to_string(),
            args.to_string(),
        );
        self.known_permission_ids.insert(request_id.clone());
        if let Some(request) = self.permission_router.get_pending(&request_id) {
            self.pending_permission_requests
                .push(permission_request_frame(request));
        }
        self.emit(BridgeEvent::PermissionRequested {
            request_id: request_id.clone(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
        });
        request_id
    }

    /// Register a permission request under an id chosen by its producer.
    ///
    /// The headless child generates request ids of its own. They are what the
    /// child and the gateway see, so the request goes on the wire under that
    /// id; the router entry is created separately and linked through
    /// `permission_aliases` for resolution.
    ///
    /// # Errors
    ///
    /// Returns an error when `request_id` was already used. A repeated id is a
    /// producer bug: silently replacing the pending entry would strand the
    /// person answering the first question and could resolve the wrong tool
    /// call.
    pub fn request_permission_with_id(
        &mut self,
        request_id: &str,
        session_id: &str,
        tool: &str,
        args: &str,
    ) -> Result<String, String> {
        if self.known_permission_ids.contains(request_id) {
            return Err(format!(
                "permission request id '{request_id}' is already known"
            ));
        }

        let router_id = self.permission_router.create_request(
            session_id.to_string(),
            tool.to_string(),
            args.to_string(),
        );
        if let Some(request) = self.permission_router.get_pending(&router_id) {
            self.pending_permission_requests
                .push(permission_request_frame_with_id(request_id, request));
        }
        self.permission_aliases
            .insert(request_id.to_string(), router_id);
        self.known_permission_ids.insert(request_id.to_string());
        self.emit(BridgeEvent::PermissionRequested {
            request_id: request_id.to_string(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
        });
        Ok(request_id.to_string())
    }

    /// Take the permission requests waiting to go out over the transport.
    #[must_use]
    pub fn take_pending_permission_requests(&mut self) -> Vec<TransportMessage> {
        std::mem::take(&mut self.pending_permission_requests)
    }

    /// Send the queued `permission_request` frames over the active transport.
    ///
    /// The serve loop owns the transport, so the launcher (or any other owner
    /// of `&mut Bridge`) calls this when it next has control. Each frame is
    /// relayed through [`PermissionFrameRelay`] first: the relay records the
    /// request as outstanding and refuses an id-less or repeated request, which
    /// is dropped rather than sent.
    pub async fn send_pending_permission_requests(&mut self) -> Result<(), String> {
        // Send head-first and only drop a frame once the transport took it:
        // a failed send leaves the request queued for the next attempt
        // instead of losing the person's chance to answer it.
        while let Some(frame) = self.pending_permission_requests.first().cloned() {
            // The relay decides whether the host may be asked at all; a frame
            // it refuses (no id, or an id already outstanding) never goes out.
            let Some(relayed) = self.permission_relay.relay_worker_frame(frame.clone()) else {
                self.pending_permission_requests.remove(0);
                continue;
            };
            // The relay keys on this id. Permission requests always carry one
            // (`permission_request_frame[_with_id]` sets it), so it is what the
            // failure path has to undo.
            let relayed_id = relayed.correlation_id.clone();

            let sent = match self.transport.as_mut() {
                Some(transport) => transport.send(relayed).await.map_err(|e| e.to_string()),
                None => Err("no transport configured".to_string()),
            };
            match sent {
                Ok(()) => {
                    self.pending_permission_requests.remove(0);
                }
                Err(error) => {
                    // The host never saw the request, so it is not outstanding:
                    // forgetting the entry lets the retry relay it again
                    // instead of refusing it as a repeat.
                    if let Some(request_id) = relayed_id {
                        self.permission_relay.forget(&request_id);
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Queue a fenced state write for a session.
    ///
    /// The frame carries the epoch registration assigned in
    /// [`WORKER_EPOCH_KEY`], which the gateway uses to reject a write from a
    /// worker that has been replaced. An unregistered worker (epoch `0`) has
    /// nothing to fence with, so the write is refused rather than sent under
    /// a guessed epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when registration has not assigned a non-zero worker
    /// epoch. The caller decides whether that is fatal; the launcher logs it
    /// once and keeps the worker running.
    pub fn queue_session_write(&mut self, session_id: &str, state: &str) -> Result<(), String> {
        let epoch = self.worker_epoch();
        if epoch == 0 {
            return Err(
                "cannot queue a session_write before registration assigned a worker epoch"
                    .to_string(),
            );
        }

        let correlation_id = format!("session_write_{}", self.next_session_write_id);
        self.next_session_write_id += 1;

        self.pending_session_writes.push(TransportMessage {
            r#type: SESSION_WRITE_TYPE.to_string(),
            correlation_id: Some(correlation_id),
            payload: serde_json::json!({
                "session_id": session_id,
                "state": state,
            })
            .to_string(),
            metadata: std::collections::HashMap::from([(
                WORKER_EPOCH_KEY.to_string(),
                epoch.to_string(),
            )]),
        });
        Ok(())
    }

    /// Take the session writes waiting to go out over the transport.
    #[must_use]
    pub fn take_pending_session_writes(&mut self) -> Vec<TransportMessage> {
        std::mem::take(&mut self.pending_session_writes)
    }

    /// Send the queued `session_write` frames over the active transport.
    ///
    /// Head-first, like permission requests: a failed send leaves the frame
    /// queued so the next flush can retry it instead of dropping the state
    /// transition.
    pub async fn send_pending_session_writes(&mut self) -> Result<(), String> {
        while let Some(frame) = self.pending_session_writes.first().cloned() {
            let transport = self.transport.as_mut().ok_or("no transport configured")?;
            transport.send(frame).await.map_err(|e| e.to_string())?;
            self.pending_session_writes.remove(0);
        }
        Ok(())
    }

    /// Forward one line of a session's output over the transport.
    pub async fn forward_session_output(
        &mut self,
        session_id: &str,
        stream: &str,
        line: &str,
    ) -> Result<(), String> {
        let transport = self.transport.as_mut().ok_or("no transport configured")?;
        let frame = TransportMessage {
            r#type: SESSION_OUTPUT_TYPE.to_string(),
            correlation_id: None,
            payload: serde_json::json!({
                "session_id": session_id,
                "stream": stream,
                "line": line,
            })
            .to_string(),
            metadata: std::collections::HashMap::new(),
        };
        transport.send(frame).await.map_err(|e| e.to_string())
    }

    /// Whether the permission request has been resolved.
    #[must_use]
    pub fn is_permission_resolved(&self, request_id: &str) -> bool {
        let request_id = self
            .permission_aliases
            .get(request_id)
            .map_or(request_id, String::as_str);
        self.permission_router.is_resolved(request_id)
    }

    /// The response for a resolved permission request.
    #[must_use]
    pub fn get_permission_response(&self, request_id: &str) -> Option<&PermissionResponse> {
        let request_id = self
            .permission_aliases
            .get(request_id)
            .map_or(request_id, String::as_str);
        self.permission_router.get_response(request_id)
    }

    // ─── Recovery state for a session ───

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

/// Walk a session to `Running` through the transitions the machine permits.
///
/// Sessions normally arrive here already running (the launcher marks them
/// when it starts the child); this also covers a session completed without
/// that call and a session that was waiting for permission when it finished.
fn drive_to_running(
    machine: &mut BridgeSessionStateMachine,
    worktree_created: bool,
) -> Result<(), String> {
    if machine.is_terminal() {
        return Err(format!(
            "cannot drive terminal session '{}' to running",
            machine.session_id
        ));
    }

    while !matches!(machine.current, BridgeSessionState::Running) {
        let next = match &machine.current {
            BridgeSessionState::Assigned => BridgeSessionState::Preparing,
            BridgeSessionState::Preparing if worktree_created => {
                BridgeSessionState::CreatingWorktree
            }
            BridgeSessionState::Preparing | BridgeSessionState::CreatingWorktree => {
                BridgeSessionState::Spawning
            }
            BridgeSessionState::Spawning | BridgeSessionState::AwaitingPermission => {
                BridgeSessionState::Running
            }
            BridgeSessionState::Running => break,
            other => {
                return Err(format!("cannot reach running from session state '{other}'"));
            }
        };
        machine
            .transition(next)
            .map_err(|e| format!("state transition failed: {e}"))?;
    }
    Ok(())
}

/// Build the frame that asks the gateway to show a pending permission.
fn permission_request_frame(request: &PendingPermission) -> TransportMessage {
    permission_request_frame_with_id(&request.id, request)
}

/// Build the frame under an explicit wire id.
fn permission_request_frame_with_id(
    request_id: &str,
    request: &PendingPermission,
) -> TransportMessage {
    TransportMessage {
        r#type: PERMISSION_REQUEST_TYPE.to_string(),
        correlation_id: Some(request_id.to_string()),
        payload: serde_json::json!({
            "request_id": request_id,
            "session_id": request.session_id,
            "tool": request.tool,
            "args": request.args,
        })
        .to_string(),
        metadata: std::collections::HashMap::new(),
    }
}

/// A `permission_response` frame.
///
/// Two spellings are accepted so a gateway can answer with the router's own
/// `PermissionResponse` shape (`{"response": {...}}`) or the simpler
/// `{"allowed": true, "always": false}`. `allowed` wins when both are present.
#[derive(Debug, Deserialize)]
struct PermissionResponseFrame {
    request_id: String,
    #[serde(default)]
    allowed: Option<bool>,
    #[serde(default)]
    always: bool,
    #[serde(default)]
    response: Option<PermissionResponse>,
}

impl PermissionResponseFrame {
    /// The request this answers and the decision to feed the router.
    fn into_response(self) -> (String, PermissionResponse) {
        let response = match (self.allowed, self.response) {
            (Some(allowed), _) => {
                if allowed {
                    PermissionResponse::Allow {
                        always: self.always,
                    }
                } else {
                    PermissionResponse::Deny
                }
            }
            (None, Some(response)) => response,
            // An answer that says neither allow nor deny is a denial: silence
            // must not become consent.
            (None, None) => PermissionResponse::Deny,
        };
        (self.request_id, response)
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

    /// Control plane that reports a fixed epoch, for the write-fence producer.
    #[derive(Debug, Default)]
    struct EpochControlPlane {
        epoch: u64,
    }

    #[async_trait::async_trait]
    impl ControlPlaneClient for EpochControlPlane {
        async fn register(&self, _env_id: &str, _max_concurrent: usize) -> Result<u64, String> {
            Ok(self.epoch)
        }

        async fn fetch_assignments(&self, _env_id: &str) -> Result<Vec<WorkAssignment>, String> {
            Ok(Vec::new())
        }

        async fn claim_assignment(
            &self,
            _env_id: &str,
            _assignment_id: &str,
        ) -> Result<ClaimOutcome, String> {
            Ok(ClaimOutcome::Claimed)
        }

        async fn report_capacity(
            &self,
            _env_id: &str,
            _report: &CapacityReport,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    fn test_bridge_with_epoch(base: &std::path::Path, epoch: u64) -> Bridge {
        let (a, _b) = InProcessTransport::create_pair("bridge".into(), "remote".into());
        Bridge::with_control_plane(
            test_config(base),
            Box::new(a),
            Arc::new(EpochControlPlane { epoch }),
        )
    }

    #[tokio::test]
    async fn a_queued_session_write_carries_the_registered_epoch_and_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge_with_epoch(tmp.path(), 7);

        // Before registration the worker has no epoch to fence with.
        assert!(bridge.queue_session_write("sess_1", "running").is_err());
        assert!(bridge.take_pending_session_writes().is_empty());

        bridge.register().await.unwrap();
        assert_eq!(bridge.worker_epoch(), 7);

        bridge.queue_session_write("sess_1", "running").unwrap();

        let frames = bridge.take_pending_session_writes();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].r#type, SESSION_WRITE_TYPE);
        assert_eq!(
            frames[0].metadata.get(WORKER_EPOCH_KEY).map(String::as_str),
            Some("7")
        );
        let payload: serde_json::Value = serde_json::from_str(&frames[0].payload).unwrap();
        assert_eq!(payload["session_id"], "sess_1");
        assert_eq!(payload["state"], "running");

        // The frame survives the wire encoding unchanged.
        let encoded = serde_json::to_string(&frames[0]).unwrap();
        let decoded: TransportMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.r#type, SESSION_WRITE_TYPE);
        assert_eq!(
            decoded.metadata.get(WORKER_EPOCH_KEY).map(String::as_str),
            Some("7")
        );
        assert_eq!(decoded.payload, frames[0].payload);
    }

    #[tokio::test]
    async fn a_zero_epoch_refuses_session_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge_with_epoch(tmp.path(), 0);

        bridge.register().await.unwrap();
        assert_eq!(bridge.worker_epoch(), 0);
        assert!(bridge.queue_session_write("sess_1", "running").is_err());
        assert!(bridge.take_pending_session_writes().is_empty());
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
    fn request_permission_with_id_keeps_the_callers_id_and_queues_one_frame() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        let id = bridge
            .request_permission_with_id("child-req-7", "sess_1", "Bash", r#"{"command":"ls"}"#)
            .unwrap();
        assert_eq!(id, "child-req-7");

        // Exactly one frame, carrying the child's id rather than the router's.
        let frames = bridge.take_pending_permission_requests();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].r#type, PERMISSION_REQUEST_TYPE);
        assert_eq!(frames[0].correlation_id.as_deref(), Some("child-req-7"));
        let payload: serde_json::Value = serde_json::from_str(&frames[0].payload).unwrap();
        assert_eq!(payload["request_id"], "child-req-7");
        assert_eq!(payload["session_id"], "sess_1");
        assert_eq!(payload["tool"], "Bash");

        // The answer arrives under the same id and reaches the router.
        assert!(
            bridge
                .respond_to_permission("child-req-7", PermissionResponse::Allow { always: false })
        );
        assert!(matches!(
            bridge.get_permission_response("child-req-7"),
            Some(PermissionResponse::Allow { always: false })
        ));
    }

    #[test]
    fn a_repeated_child_request_id_is_rejected_without_queueing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        bridge
            .request_permission_with_id("child-req-7", "sess_1", "Bash", "ls")
            .unwrap();
        let _ = bridge.take_pending_permission_requests();

        assert!(
            bridge
                .request_permission_with_id("child-req-7", "sess_1", "Bash", "ls")
                .is_err()
        );
        assert!(bridge.take_pending_permission_requests().is_empty());
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

    #[tokio::test]
    async fn a_permission_response_frame_resolves_the_pending_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (bridge_transport, mut gateway) =
            InProcessTransport::create_pair("bridge".into(), "gateway".into());
        gateway.connect().await.unwrap();
        let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
        bridge.connect().await.unwrap();

        let request_id = bridge.request_permission("sess_1", "Bash", r#"{"command":"ls"}"#);

        // The request reaches the gateway, carrying what the person must see.
        bridge.send_pending_permission_requests().await.unwrap();
        let request = gateway.receive().await.unwrap();
        assert_eq!(request.r#type, PERMISSION_REQUEST_TYPE);
        assert_eq!(request.correlation_id.as_deref(), Some(request_id.as_str()));
        let payload: serde_json::Value = serde_json::from_str(&request.payload).unwrap();
        assert_eq!(payload["request_id"], request_id);
        assert_eq!(payload["session_id"], "sess_1");
        assert_eq!(payload["tool"], "Bash");
        // Sending relayed the request: the relay now expects an answer for it.
        assert_eq!(bridge.outstanding_relayed_permissions(), 1);

        // The answer resolves the router, and an approval populates the gate
        // the relay endpoint checks.
        let reply = bridge
            .handle_transport_message(&TransportMessage {
                r#type: PERMISSION_RESPONSE_TYPE.into(),
                correlation_id: Some(request_id.clone()),
                payload: serde_json::json!({
                    "request_id": request_id,
                    "allowed": true,
                    "always": false,
                })
                .to_string(),
                metadata: std::collections::HashMap::new(),
            })
            .unwrap();
        assert_eq!(reply.r#type, PERMISSION_RESULT_TYPE);
        let payload: serde_json::Value = serde_json::from_str(&reply.payload).unwrap();
        assert_eq!(payload["resolved"], true);
        assert!(bridge.is_permission_resolved(&request_id));
        assert_eq!(bridge.outstanding_relay_approvals(), 1);
        assert_eq!(bridge.outstanding_relayed_permissions(), 0);
    }

    /// The answer to a request, in the shape the gateway sends it back.
    fn permission_answer(request_id: &str, allowed: bool) -> TransportMessage {
        TransportMessage {
            r#type: PERMISSION_RESPONSE_TYPE.into(),
            correlation_id: Some(request_id.to_string()),
            payload: serde_json::json!({
                "request_id": request_id,
                "allowed": allowed,
                "always": false,
            })
            .to_string(),
            metadata: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn a_duplicate_permission_response_is_refused_after_the_first_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let (bridge_transport, mut gateway) =
            InProcessTransport::create_pair("bridge".into(), "gateway".into());
        gateway.connect().await.unwrap();
        let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
        bridge.connect().await.unwrap();

        let request_id = bridge.request_permission("sess_1", "Bash", "ls");
        bridge.send_pending_permission_requests().await.unwrap();
        let _ = gateway.receive().await.unwrap();
        assert_eq!(bridge.outstanding_relayed_permissions(), 1);

        let first = bridge
            .handle_transport_message(&permission_answer(&request_id, true))
            .unwrap();
        let first_payload: serde_json::Value = serde_json::from_str(&first.payload).unwrap();
        assert_eq!(first_payload["resolved"], true);
        assert_eq!(bridge.outstanding_relayed_permissions(), 0);

        // The replay has nothing left to answer: the relay refuses it, so the
        // router is never asked a second time and the first decision stands.
        let replay = bridge
            .handle_transport_message(&permission_answer(&request_id, false))
            .unwrap();
        let replay_payload: serde_json::Value = serde_json::from_str(&replay.payload).unwrap();
        assert_eq!(replay_payload["resolved"], false);
        assert!(matches!(
            bridge.get_permission_response(&request_id),
            Some(PermissionResponse::Allow { always: false })
        ));
    }

    #[tokio::test]
    async fn a_permission_response_after_the_timeout_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (bridge_transport, mut gateway) =
            InProcessTransport::create_pair("bridge".into(), "gateway".into());
        gateway.connect().await.unwrap();
        let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
        bridge.connect().await.unwrap();

        let request_id = bridge.request_permission("sess_1", "Bash", "rm -rf /tmp/x");
        bridge.send_pending_permission_requests().await.unwrap();
        let _ = gateway.receive().await.unwrap();
        assert_eq!(bridge.outstanding_relayed_permissions(), 1);

        // The bounded wait elapses: the request is denied locally and stops
        // being outstanding.
        assert!(bridge.expire_permission(&request_id));
        assert_eq!(bridge.outstanding_relayed_permissions(), 0);

        // A late answer is refused by the relay and cannot overwrite the
        // denial the child already received.
        let reply = bridge
            .handle_transport_message(&permission_answer(&request_id, true))
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&reply.payload).unwrap();
        assert_eq!(payload["resolved"], false);
        assert!(matches!(
            bridge.get_permission_response(&request_id),
            Some(PermissionResponse::Deny)
        ));
    }

    #[tokio::test]
    async fn an_outstanding_permission_survives_a_transport_reconnect() {
        let tmp = tempfile::tempdir().unwrap();
        let (bridge_transport, mut gateway) =
            InProcessTransport::create_pair("bridge".into(), "gateway".into());
        gateway.connect().await.unwrap();
        let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
        bridge.connect().await.unwrap();

        let request_id = bridge.request_permission("sess_1", "Bash", "ls");
        bridge.send_pending_permission_requests().await.unwrap();
        let _ = gateway.receive().await.unwrap();

        // A reconnect is not an answer. Skip the backoff wait (the reconnect
        // itself is not what this test is about) and check the outstanding
        // request is still the one an answer may resolve.
        bridge.reconnect_deadline = Some(tokio::time::Instant::now());
        bridge.reconnect_with_resume().await.unwrap();
        assert_eq!(bridge.outstanding_relayed_permissions(), 1);

        let reply = bridge
            .handle_transport_message(&permission_answer(&request_id, false))
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&reply.payload).unwrap();
        assert_eq!(payload["resolved"], true);
        assert_eq!(bridge.outstanding_relayed_permissions(), 0);
    }

    #[tokio::test]
    async fn a_repeated_permission_request_is_relayed_only_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (bridge_transport, mut gateway) =
            InProcessTransport::create_pair("bridge".into(), "gateway".into());
        gateway.connect().await.unwrap();
        let mut bridge = Bridge::new(test_config(tmp.path()), Box::new(bridge_transport));
        bridge.connect().await.unwrap();

        // A producer bug queued the same request frame twice; the relay sends
        // it to the host once and refuses the repeat.
        let frame = TransportMessage {
            r#type: PERMISSION_REQUEST_TYPE.into(),
            correlation_id: Some("dup-1".into()),
            payload: serde_json::json!({
                "request_id": "dup-1",
                "session_id": "s1",
                "tool": "Bash",
                "args": "ls",
            })
            .to_string(),
            metadata: std::collections::HashMap::new(),
        };
        bridge.pending_permission_requests.push(frame.clone());
        bridge.pending_permission_requests.push(frame);
        bridge.send_pending_permission_requests().await.unwrap();

        let first = gateway.receive().await.unwrap();
        assert_eq!(first.correlation_id.as_deref(), Some("dup-1"));
        assert_eq!(bridge.outstanding_relayed_permissions(), 1);
        assert!(
            gateway
                .receive_timeout(std::time::Duration::from_millis(50))
                .await
                .is_err(),
            "the repeated request must not reach the host"
        );
    }

    #[tokio::test]
    async fn an_unknown_permission_response_resolves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());

        let reply = bridge
            .handle_transport_message(&TransportMessage {
                r#type: "approval".into(),
                correlation_id: None,
                payload: serde_json::json!({
                    "request_id": "perm_missing",
                    "allowed": false,
                })
                .to_string(),
                metadata: std::collections::HashMap::new(),
            })
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&reply.payload).unwrap();
        assert_eq!(payload["resolved"], false);
        assert_eq!(bridge.outstanding_relay_approvals(), 0);
    }

    fn init_git_repo(path: &std::path::Path) {
        let git = |args: &[&str]| {
            coderex_core::git_hardening::git_command()
                .args(args)
                .current_dir(path)
                .output()
                .expect("git command failed");
        };
        git(&["init", "--quiet"]);
        std::fs::write(path.join("README"), "root").unwrap();
        git(&["add", "README"]);
        coderex_core::git_hardening::git_command()
            .args(["commit", "--quiet", "-m", "init"])
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .current_dir(path)
            .output()
            .expect("git commit failed");
    }

    fn session_history(bridge: &Bridge, session_id: &str) -> Vec<BridgeSessionState> {
        bridge
            .sessions
            .get(session_id)
            .expect("session is tracked")
            .state_machine
            .history
            .clone()
    }

    #[tokio::test]
    async fn spawn_session_records_preparing_and_spawning_without_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        let mut bridge = Bridge::new(
            test_config(tmp.path()),
            Box::new(InProcessTransport::create_pair("b".into(), "r".into()).0),
        );
        bridge.connect().await.unwrap();

        let assignment = WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: tmp.path().join("work").to_str().unwrap().into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::SingleSession,
        };
        bridge.spawn_session(&assignment).await.unwrap();

        assert_eq!(
            session_history(&bridge, "s1"),
            vec![
                BridgeSessionState::Assigned,
                BridgeSessionState::Preparing,
                BridgeSessionState::Spawning,
            ],
            "a directory session must not enter CreatingWorktree"
        );
    }

    #[tokio::test]
    async fn spawn_session_records_creating_worktree_when_one_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktrees = tmp.path().join("worktrees");
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&worktrees).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        init_git_repo(&repo);

        let mut config = test_config(tmp.path());
        config.repo_root = repo;
        config.worktree_base = worktrees;
        config.session_base = sessions;
        let mut bridge = Bridge::new(
            config,
            Box::new(InProcessTransport::create_pair("b".into(), "r".into()).0),
        );
        bridge.connect().await.unwrap();

        let assignment = WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "wt".into(),
            working_directory: tmp.path().to_str().unwrap().into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::Worktree,
        };
        let result = bridge.spawn_session(&assignment).await.unwrap();

        assert!(result.worktree_created);
        assert_eq!(
            session_history(&bridge, "wt"),
            vec![
                BridgeSessionState::Assigned,
                BridgeSessionState::Preparing,
                BridgeSessionState::CreatingWorktree,
                BridgeSessionState::Spawning,
            ]
        );
    }

    #[tokio::test]
    async fn a_permission_wait_is_a_round_trip_through_awaiting_permission() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        let mut bridge = test_bridge(tmp.path());
        let assignment = WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: tmp.path().join("work").to_str().unwrap().into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::SingleSession,
        };
        bridge.spawn_session(&assignment).await.unwrap();
        bridge.mark_session_running("s1").unwrap();

        bridge.begin_awaiting_permission("s1").unwrap();
        assert_eq!(
            bridge.session_state("s1"),
            Some(&BridgeSessionState::AwaitingPermission)
        );

        bridge.end_awaiting_permission("s1").unwrap();
        assert_eq!(
            session_history(&bridge, "s1"),
            vec![
                BridgeSessionState::Assigned,
                BridgeSessionState::Preparing,
                BridgeSessionState::Spawning,
                BridgeSessionState::Running,
                BridgeSessionState::AwaitingPermission,
                BridgeSessionState::Running,
            ]
        );

        // An early move is still refused: only a running session can wait.
        bridge.begin_awaiting_permission("s1").unwrap();
        assert!(
            bridge
                .sessions
                .get_mut("s1")
                .unwrap()
                .state_machine
                .transition(BridgeSessionState::Archiving)
                .is_err(),
            "a waiting session must return to Running before archiving"
        );
    }

    #[tokio::test]
    async fn fail_session_enters_failed_through_the_validated_graph() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        let mut bridge = test_bridge(tmp.path());
        let assignment = WorkAssignment {
            assignment_id: "a1".into(),
            session_id: "s1".into(),
            working_directory: tmp.path().join("work").to_str().unwrap().into(),
            prompt: "test".into(),
            spawn_mode: SpawnMode::SingleSession,
        };
        bridge.spawn_session(&assignment).await.unwrap();
        bridge.mark_session_running("s1").unwrap();

        bridge.fail_session("s1", "child exited 1".into());

        assert!(matches!(
            bridge.sessions.get("s1").unwrap().state_machine.current,
            BridgeSessionState::Failed(ref reason) if reason == "child exited 1"
        ));
    }

    #[tokio::test]
    async fn an_expired_permission_is_denied_and_a_late_answer_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = test_bridge(tmp.path());
        let request_id = bridge.request_permission("s1", "Bash", "rm -rf /tmp/x");

        assert!(bridge.expire_permission(&request_id));
        assert!(matches!(
            bridge.get_permission_response(&request_id),
            Some(PermissionResponse::Deny)
        ));
        // The timeout already answered; a gateway answer that arrives later
        // must not overwrite it.
        assert!(
            !bridge.respond_to_permission(&request_id, PermissionResponse::Allow { always: true })
        );
        assert!(matches!(
            bridge.get_permission_response(&request_id),
            Some(PermissionResponse::Deny)
        ));
    }

    /// A socket that drops is retried, and the resume frame carries the point
    /// the downstream stream reached.
    #[tokio::test]
    async fn a_dropped_transport_reconnects_with_the_resume_cursor() {
        let state = Arc::new(std::sync::Mutex::new(ScriptedState {
            connects: 0,
            fail_connect: false,
            sent: Vec::new(),
            script: std::collections::VecDeque::from([
                Ok(TransportMessage {
                    r#type: "heartbeat".into(),
                    correlation_id: Some("h7".into()),
                    payload: "{}".into(),
                    metadata: std::collections::HashMap::from([(
                        SEQUENCE_NUM_KEY.to_string(),
                        "7".to_string(),
                    )]),
                }),
                Err(crate::transport::TransportError::ConnectionLost(
                    "socket dropped".into(),
                )),
            ]),
        }));

        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = Bridge::new(
            test_config(tmp.path()),
            Box::new(ScriptedTransport::new("scripted", Arc::clone(&state))),
        );
        bridge.reconnector = WsReconnector::new(3);
        bridge.reconnector.base_delay_ms = 0;
        bridge.connect().await.unwrap();

        let stop = bridge.stop_handle();
        let serving = tokio::spawn(async move { bridge.serve_transport().await });

        let resume = wait_for_sent(&state, RESUME_TYPE).await;
        let payload: serde_json::Value = serde_json::from_str(&resume.payload).unwrap();
        assert_eq!(payload["from_sequence_num"], 8);
        assert_eq!(
            resume.metadata.get(LAST_EVENT_ID_KEY).map(String::as_str),
            Some("7")
        );

        // A clean stop after reconnecting ends the loop rather than retrying.
        stop.stop();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), serving)
            .await
            .expect("the serve loop should stop")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    /// Exhausted reconnect attempts are terminal: the serve loop returns an
    /// error instead of spinning on a dead socket.
    #[tokio::test]
    async fn exhausted_reconnects_are_terminal() {
        let state = Arc::new(std::sync::Mutex::new(ScriptedState {
            connects: 0,
            fail_connect: true,
            sent: Vec::new(),
            script: std::collections::VecDeque::from([Err(
                crate::transport::TransportError::ConnectionLost("socket dropped".into()),
            )]),
        }));

        let tmp = tempfile::tempdir().unwrap();
        let mut bridge = Bridge::new(
            test_config(tmp.path()),
            Box::new(ScriptedTransport::new("scripted", Arc::clone(&state))),
        );
        bridge.reconnector = WsReconnector::new(2);
        bridge.reconnector.base_delay_ms = 0;

        let error = bridge.serve_transport().await.unwrap_err();
        assert!(error.contains("exhausted"), "{error}");
        assert_eq!(
            state.lock().unwrap().connects,
            2,
            "the initial connect is skipped; exactly the bounded attempts run"
        );
    }

    /// Wait until a frame of `kind` was sent by the bridge.
    async fn wait_for_sent(
        state: &Arc<std::sync::Mutex<ScriptedState>>,
        kind: &str,
    ) -> TransportMessage {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(frame) = state
                    .lock()
                    .unwrap()
                    .sent
                    .iter()
                    .find(|frame| frame.r#type == kind)
                {
                    return frame.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the bridge never sent a {kind} frame"))
    }

    #[derive(Debug)]
    struct ScriptedState {
        connects: usize,
        fail_connect: bool,
        sent: Vec<TransportMessage>,
        script:
            std::collections::VecDeque<Result<TransportMessage, crate::transport::TransportError>>,
    }

    /// A transport whose receive results are scripted, so a dropped socket can
    /// be exercised without a network.
    #[derive(Debug)]
    struct ScriptedTransport {
        id: String,
        state: Arc<std::sync::Mutex<ScriptedState>>,
    }

    impl ScriptedTransport {
        fn new(id: &str, state: Arc<std::sync::Mutex<ScriptedState>>) -> Self {
            Self {
                id: id.to_string(),
                state,
            }
        }
    }

    #[async_trait::async_trait]
    impl Transport for ScriptedTransport {
        async fn connect(&mut self) -> Result<(), crate::transport::TransportError> {
            let mut state = self.state.lock().unwrap();
            state.connects += 1;
            if state.fail_connect {
                Err(crate::transport::TransportError::ConnectionLost(
                    "refused by test".into(),
                ))
            } else {
                Ok(())
            }
        }

        async fn disconnect(&mut self) -> Result<(), crate::transport::TransportError> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            true
        }

        async fn send(
            &mut self,
            message: TransportMessage,
        ) -> Result<(), crate::transport::TransportError> {
            self.state.lock().unwrap().sent.push(message);
            Ok(())
        }

        async fn receive(&mut self) -> Result<TransportMessage, crate::transport::TransportError> {
            // Drop the guard before awaiting: the worker's future must be Send.
            let next = self.state.lock().unwrap().script.pop_front();
            match next {
                Some(result) => result,
                None => std::future::pending().await,
            }
        }

        fn connection_id(&self) -> &str {
            &self.id
        }
    }
}
