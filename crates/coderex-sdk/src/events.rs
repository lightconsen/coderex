//! SDK structured event stream, permission tracking, and elicitation.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use coderex_core::types::{EngineEvent, ToolCall, ToolResult};

// ── Events ──────────────────────────────────────────────────────────────

/// Events emitted by the SDK.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SdkEvent {
    /// A text delta from the assistant.
    TextDelta(String),
    /// A tool call started.
    ToolCallStart {
        /// The tool call that was initiated.
        tool_call: ToolCall,
    },
    /// A tool call ended.
    ToolCallEnd {
        /// The result of the tool call.
        result: ToolResult,
    },
    /// A compact boundary was inserted.
    CompactBoundary,
    /// A permission request.
    PermissionRequest {
        /// The unique request identifier.
        id: String,
        /// The tool name requesting permission.
        tool: String,
        /// The arguments passed to the tool.
        args: serde_json::Value,
    },
    /// A permission request was resolved.
    PermissionResolved {
        /// The unique request identifier.
        id: String,
        /// Whether it was allowed.
        allowed: bool,
    },
    /// An elicitation request.
    ElicitationRequest {
        /// The unique request identifier.
        id: String,
        /// The elicitation message.
        message: String,
    },
    /// An elicitation was responded to.
    ElicitationResolved {
        /// The unique request identifier.
        id: String,
    },
    /// Session ended.
    SessionEnd,
    /// Engine event from the core.
    Engine(EngineEvent),
}

// ── Event Stream ────────────────────────────────────────────────────────

/// Broadcast event stream for SDK consumers.
#[derive(Debug)]
pub struct SdkEventStream {
    /// The broadcast sender.
    tx: broadcast::Sender<SdkEvent>,
}

impl SdkEventStream {
    /// Create a new event stream.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    /// Emit an event to all listeners.
    pub fn emit(&self, event: SdkEvent) {
        // Ignore if no receivers
        let _ = self.tx.send(event);
    }

    /// Subscribe to the event stream.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SdkEvent> {
        self.tx.subscribe()
    }

    /// Count of active subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for SdkEventStream {
    fn default() -> Self {
        Self::new()
    }
}

// ── Permission Tracking ─────────────────────────────────────────────────

/// Status of a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionStatus {
    /// Waiting for a response.
    Pending,
    /// Permission granted.
    Allowed,
    /// Permission denied.
    Denied,
}

/// A tracked permission request.
#[derive(Debug, Clone)]
pub struct TrackedPermission {
    /// Unique request ID.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Tool arguments.
    pub args: serde_json::Value,
    /// Current status.
    pub status: PermissionStatus,
}

/// Tracks permission requests and their resolutions.
#[derive(Debug)]
pub struct PermissionTracker {
    pending: HashMap<String, TrackedPermission>,
    resolved: HashMap<String, TrackedPermission>,
    next_id: usize,
}

impl Default for PermissionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionTracker {
    /// Create a new permission tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            resolved: HashMap::new(),
            next_id: 0,
        }
    }

    /// Create a new pending permission request.
    pub fn create(&mut self, tool: String, args: serde_json::Value) -> String {
        let id = format!("perm_{}", self.next_id);
        self.next_id += 1;
        let tracked = TrackedPermission {
            id: id.clone(),
            tool,
            args,
            status: PermissionStatus::Pending,
        };
        self.pending.insert(id.clone(), tracked);
        id
    }

    /// Resolve a permission request.
    pub fn resolve(&mut self, id: &str, allowed: bool) -> bool {
        if let Some(mut perm) = self.pending.remove(id) {
            perm.status = if allowed {
                PermissionStatus::Allowed
            } else {
                PermissionStatus::Denied
            };
            self.resolved.insert(id.to_string(), perm);
            true
        } else {
            false
        }
    }

    /// Get a permission request by ID (checks both pending and resolved).
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&TrackedPermission> {
        self.pending.get(id).or_else(|| self.resolved.get(id))
    }

    /// List all pending (unresolved) requests.
    #[must_use]
    pub fn pending(&self) -> Vec<&TrackedPermission> {
        self.pending.values().collect()
    }

    /// List all resolved requests.
    #[must_use]
    pub fn resolved(&self) -> Vec<&TrackedPermission> {
        self.resolved.values().collect()
    }

    /// Number of pending requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

// ── Elicitation ─────────────────────────────────────────────────────────

/// Response to an elicitation prompt.
#[derive(Debug, Clone)]
pub struct ElicitationResponse {
    /// The elicitation ID.
    pub id: String,
    /// Whether the user confirmed.
    pub confirmed: bool,
    /// Optional user-provided notes.
    pub notes: Option<String>,
}

/// Tracks elicitation requests.
#[derive(Debug)]
pub struct ElicitationTracker {
    pending: HashMap<String, String>,
    resolved: HashMap<String, ElicitationResponse>,
    next_id: usize,
}

impl Default for ElicitationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ElicitationTracker {
    /// Create a new elicitation tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            resolved: HashMap::new(),
            next_id: 0,
        }
    }

    /// Create a new elicitation request.
    pub fn create(&mut self, message: String) -> String {
        let id = format!("elicit_{}", self.next_id);
        self.next_id += 1;
        self.pending.insert(id.clone(), message);
        id
    }

    /// Resolve an elicitation request.
    pub fn resolve(&mut self, id: &str, confirmed: bool, notes: Option<String>) -> bool {
        if let Some(_message) = self.pending.remove(id) {
            let resp = ElicitationResponse {
                id: id.to_string(),
                confirmed,
                notes,
            };
            self.resolved.insert(id.to_string(), resp);
            true
        } else {
            false
        }
    }

    /// Get the message for a pending elicitation.
    #[must_use]
    pub fn get_message(&self, id: &str) -> Option<&String> {
        self.pending.get(id)
    }

    /// Get the response for a resolved elicitation.
    #[must_use]
    pub fn get_response(&self, id: &str) -> Option<&ElicitationResponse> {
        self.resolved.get(id)
    }

    /// Count of pending elicitations.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

// ── SdkSession ──────────────────────────────────────────────────────────

/// Unified SDK session combining event stream, permission tracking, and elicitation.
#[derive(Debug)]
pub struct SdkSession {
    /// Session ID.
    pub id: String,
    /// Event broadcast channel.
    pub events: Arc<SdkEventStream>,
    /// Permission request tracker.
    pub permissions: Arc<Mutex<PermissionTracker>>,
    /// Elicitation tracker.
    pub elicitations: Arc<Mutex<ElicitationTracker>>,
}

impl SdkSession {
    /// Create a new SDK session.
    #[must_use]
    pub fn new(id: String) -> Self {
        Self {
            id,
            events: Arc::new(SdkEventStream::new()),
            permissions: Arc::new(Mutex::new(PermissionTracker::new())),
            elicitations: Arc::new(Mutex::new(ElicitationTracker::new())),
        }
    }

    /// Emit an event.
    pub fn emit(&self, event: SdkEvent) {
        self.events.emit(event);
    }

    /// Subscribe to events.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<SdkEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_stream_broadcast() {
        let stream = SdkEventStream::new();
        let mut rx = stream.subscribe();
        stream.emit(SdkEvent::SessionEnd);
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, SdkEvent::SessionEnd));
    }

    #[test]
    fn event_stream_multiple_subscribers() {
        let stream = SdkEventStream::new();
        let mut rx1 = stream.subscribe();
        let mut rx2 = stream.subscribe();
        stream.emit(SdkEvent::TextDelta("hello".to_string()));
        assert!(matches!(rx1.try_recv(), Ok(SdkEvent::TextDelta(ref s)) if s == "hello"));
        assert!(matches!(rx2.try_recv(), Ok(SdkEvent::TextDelta(ref s)) if s == "hello"));
    }

    #[test]
    fn permission_tracker_lifecycle() {
        let mut tracker = PermissionTracker::new();
        let id = tracker.create("Bash".to_string(), serde_json::json!({"command": "ls"}));
        assert_eq!(tracker.pending_count(), 1);

        let perm = tracker.get(&id).unwrap();
        assert_eq!(perm.status, PermissionStatus::Pending);

        tracker.resolve(&id, true);
        assert_eq!(tracker.pending_count(), 0);

        let perm = tracker.get(&id).unwrap();
        assert_eq!(perm.status, PermissionStatus::Allowed);
    }

    #[test]
    fn permission_resolve_nonexistent() {
        let mut tracker = PermissionTracker::new();
        assert!(!tracker.resolve("nope", true));
    }

    #[test]
    fn elicitation_tracker_lifecycle() {
        let mut tracker = ElicitationTracker::new();
        let id = tracker.create("Confirm deletion?".to_string());
        assert_eq!(tracker.pending_count(), 1);

        let msg = tracker.get_message(&id).unwrap();
        assert_eq!(msg, "Confirm deletion?");

        tracker.resolve(&id, true, Some("ok".to_string()));
        assert_eq!(tracker.pending_count(), 0);

        let resp = tracker.get_response(&id).unwrap();
        assert!(resp.confirmed);
        assert_eq!(resp.notes.as_deref(), Some("ok"));
    }

    #[test]
    fn elicitation_resolve_nonexistent() {
        let mut tracker = ElicitationTracker::new();
        assert!(!tracker.resolve("nope", false, None));
    }

    #[test]
    fn sdk_session_emits_events() {
        let session = SdkSession::new("s1".to_string());
        let mut rx = session.subscribe_events();
        session.emit(SdkEvent::CompactBoundary);
        assert!(matches!(rx.try_recv(), Ok(SdkEvent::CompactBoundary)));
    }

    #[test]
    fn permission_resolved_lists() {
        let mut tracker = PermissionTracker::new();
        let id = tracker.create("Read".to_string(), serde_json::json!({}));
        tracker.resolve(&id, false);
        assert!(tracker.pending().is_empty());
        assert_eq!(tracker.resolved().len(), 1);
        assert_eq!(tracker.resolved()[0].status, PermissionStatus::Denied);
    }

    #[test]
    fn elicitations_resolved_list() {
        let mut tracker = ElicitationTracker::new();
        let id = tracker.create("Are you sure?".to_string());
        tracker.resolve(&id, false, None);
        let resp = tracker.get_response(&id).unwrap();
        assert!(!resp.confirmed);
        assert!(resp.notes.is_none());
    }
}
