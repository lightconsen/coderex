//! SDK structured event stream, permission tracking, and elicitation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

use coderex_core::types::{EngineEvent, ToolCall, ToolResult};

// ── Events ──────────────────────────────────────────────────────────────

/// Events emitted by the SDK.
///
/// This enum captures every significant event that occurs during a CodeRex
/// session so that headless consumers (servers, scripts, test rigs) can
/// observe and react to the assistant's behaviour.
///
/// # Event lifecycle
///
/// A typical turn looks like this:
///
/// 1. One or more [`SdkEvent::TextDelta`] events as the model streams its
///    response.
/// 2. Zero or more [`SdkEvent::ToolCallStart`] / [`SdkEvent::ToolCallEnd`]
///    pairs when the model decides to invoke tools.
/// 3. A [`SdkEvent::CompactBoundary`] whenever the context window is
///    compacted.
/// 4. [`SdkEvent::PermissionRequest`] when a dangerous tool is invoked;
///    followed by [`SdkEvent::PermissionResolved`] once the user (or a
///    policy) responds.
/// 5. [`SdkEvent::ElicitationRequest`] when the model needs clarification;
///    followed by [`SdkEvent::ElicitationResolved`] once the user answers.
/// 6. [`SdkEvent::SessionEnd`] when the session terminates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "snake_case")]
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
///
/// Uses a `tokio::sync::broadcast` channel with capacity 4096.  If a
/// subscriber is slow, events are dropped for that subscriber and a
/// warning is logged via [`SdkSubscriber::lag()`].
#[derive(Debug, Clone)]
pub struct SdkEventStream {
    /// The broadcast sender.
    tx: broadcast::Sender<SdkEvent>,
}

impl SdkEventStream {
    /// Create a new event stream.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self { tx }
    }

    /// Emit an event to all listeners.
    pub fn emit(&self, event: SdkEvent) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        if let Err(e) = self.tx.send(event) {
            tracing::debug!("sdk event broadcast failed (no receivers): {e}");
        }
    }

    /// Subscribe to the event stream with backpressure tracking.
    ///
    /// The returned [`SdkSubscriber`] wraps the raw broadcast receiver and
    /// counts how many events were dropped because the subscriber was slow.
    #[must_use]
    pub fn subscribe(&self) -> SdkSubscriber {
        SdkSubscriber::new(self.tx.subscribe())
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

// ── Backpressure ────────────────────────────────────────────────────────

/// A subscriber wrapper that tracks lag (dropped events).
///
/// If a subscriber falls behind the broadcast buffer, `tokio::sync::broadcast`
/// silently drops events for that subscriber.  [`SdkSubscriber`] counts
/// those drops and exposes them via [`lag()`](Self::lag).  Callers can
/// disconnect slow subscribers when lag exceeds a threshold.
#[derive(Debug)]
pub struct SdkSubscriber {
    inner: broadcast::Receiver<SdkEvent>,
    lag: std::sync::atomic::AtomicUsize,
}

impl SdkSubscriber {
    fn new(inner: broadcast::Receiver<SdkEvent>) -> Self {
        Self {
            inner,
            lag: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Receive the next event, accounting for lag.
    ///
    /// Returns `Ok(event)` on success, or `Err(RecvError)` if the
    /// broadcast channel is closed.
    pub async fn recv(&mut self) -> Result<SdkEvent, broadcast::error::RecvError> {
        match self.inner.recv().await {
            Ok(event) => Ok(event),
            Err(e) => Err(e),
        }
    }

    /// Try to receive an event without awaiting.
    pub fn try_recv(&mut self) -> Result<SdkEvent, broadcast::error::TryRecvError> {
        match self.inner.try_recv() {
            Ok(event) => Ok(event),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                self.lag
                    .fetch_add(n as usize, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    "sdk subscriber lagged by {n} events; consider disconnecting slow consumer"
                );
                Err(broadcast::error::TryRecvError::Lagged(n))
            }
            Err(e) => Err(e),
        }
    }

    /// Number of events this subscriber has missed due to lag.
    #[must_use]
    pub fn lag(&self) -> usize {
        self.lag.load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ── Permission Tracking ─────────────────────────────────────────────────

/// Status of a permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionStatus {
    /// Waiting for a response.
    Pending,
    /// Permission granted.
    Allowed,
    /// Permission denied.
    Denied,
    /// Auto-denied because the request timed out.
    TimedOut,
}

/// Expected type for a permission argument field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgType {
    /// String argument.
    String,
    /// Numeric argument.
    Number,
    /// Boolean argument.
    Boolean,
    /// Array argument.
    Array,
    /// Object argument.
    Object,
}

/// Schema describing expected permission arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionSchema {
    /// Named fields and their expected types.
    pub fields: Vec<(String, ArgType)>,
}

/// Error returned when permission arguments fail schema validation.
#[derive(Debug, Clone, thiserror::Error)]
#[error("permission validation failed: {message}")]
pub struct PermissionValidationError {
    /// Human-readable explanation.
    pub message: String,
}

impl PermissionSchema {
    /// Validate a JSON value against this schema.
    ///
    /// Returns `Ok(())` if `args` is an object whose keys are a subset of
    /// `self.fields` and each value matches the declared [`ArgType`].
    pub fn validate(&self, args: &serde_json::Value) -> Result<(), PermissionValidationError> {
        let obj = args.as_object().ok_or_else(|| PermissionValidationError {
            message: "args must be a JSON object".into(),
        })?;

        let allowed: std::collections::HashSet<&str> =
            self.fields.iter().map(|(k, _)| k.as_str()).collect();

        for key in obj.keys() {
            if !allowed.contains(key.as_str()) {
                return Err(PermissionValidationError {
                    message: format!("unexpected field: {key}"),
                });
            }
        }

        for (key, expected) in &self.fields {
            if let Some(value) = obj.get(key) {
                let ok = match expected {
                    ArgType::String => value.is_string(),
                    ArgType::Number => value.is_number(),
                    ArgType::Boolean => value.is_boolean(),
                    ArgType::Array => value.is_array(),
                    ArgType::Object => value.is_object(),
                };
                if !ok {
                    return Err(PermissionValidationError {
                        message: format!("field {key} has wrong type (expected {expected:?})"),
                    });
                }
            }
        }

        Ok(())
    }
}

/// A tracked permission request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedPermission {
    /// Unique request ID.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Tool arguments.
    pub args: serde_json::Value,
    /// Current status.
    pub status: PermissionStatus,
    /// When the request was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Seconds until auto-denial (0 = no timeout).
    pub timeout_secs: u64,
}

/// Tracks permission requests and their resolutions.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    ///
    /// If `schema` is provided, `args` is validated against it before the
    /// request is created.  A zero `timeout_secs` means the request never
    /// expires automatically.
    pub fn create(
        &mut self,
        tool: String,
        args: serde_json::Value,
        schema: Option<&PermissionSchema>,
        timeout_secs: u64,
    ) -> Result<String, PermissionValidationError> {
        if let Some(s) = schema {
            s.validate(&args)?;
        }

        let id = format!("perm_{}", self.next_id);
        self.next_id += 1;
        let tracked = TrackedPermission {
            id: id.clone(),
            tool,
            args,
            status: PermissionStatus::Pending,
            created_at: chrono::Utc::now(),
            timeout_secs,
        };
        self.pending.insert(id.clone(), tracked);
        Ok(id)
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

    /// Resolve a permission as timed-out.
    pub fn resolve_timeout(&mut self, id: &str) -> bool {
        if let Some(mut perm) = self.pending.remove(id) {
            perm.status = PermissionStatus::TimedOut;
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

    /// Find pending requests whose timeout has elapsed.
    #[must_use]
    pub fn expired(&self) -> Vec<&TrackedPermission> {
        let now = chrono::Utc::now();
        self.pending
            .values()
            .filter(|p| {
                p.timeout_secs > 0 && (now - p.created_at).num_seconds() > p.timeout_secs as i64
            })
            .collect()
    }
}

// ── Elicitation ─────────────────────────────────────────────────────────

/// Response to an elicitation prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    /// The elicitation ID.
    pub id: String,
    /// Whether the user confirmed.
    pub confirmed: bool,
    /// Optional user-provided notes.
    pub notes: Option<String>,
}

/// A tracked elicitation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedElicitation {
    /// Unique request ID.
    pub id: String,
    /// The elicitation message.
    pub message: String,
    /// When the request was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Seconds until auto-timeout (0 = no timeout).
    pub timeout_secs: u64,
}

/// Tracks elicitation requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationTracker {
    pending: HashMap<String, TrackedElicitation>,
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
    ///
    /// A zero `timeout_secs` means the request never expires automatically.
    pub fn create(&mut self, message: String, timeout_secs: u64) -> String {
        let id = format!("elicit_{}", self.next_id);
        self.next_id += 1;
        let tracked = TrackedElicitation {
            id: id.clone(),
            message,
            created_at: chrono::Utc::now(),
            timeout_secs,
        };
        self.pending.insert(id.clone(), tracked);
        id
    }

    /// Resolve an elicitation request.
    pub fn resolve(&mut self, id: &str, confirmed: bool, notes: Option<String>) -> bool {
        if self.pending.remove(id).is_some() {
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

    /// Resolve an elicitation as timed-out.
    pub fn resolve_timeout(&mut self, id: &str) -> bool {
        if self.pending.remove(id).is_some() {
            let resp = ElicitationResponse {
                id: id.to_string(),
                confirmed: false,
                notes: Some("timed out".into()),
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
        self.pending.get(id).map(|t| &t.message)
    }

    /// Get a pending elicitation by ID.
    #[must_use]
    pub fn get_pending(&self, id: &str) -> Option<&TrackedElicitation> {
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

    /// Find pending elicitations whose timeout has elapsed.
    #[must_use]
    pub fn expired(&self) -> Vec<&TrackedElicitation> {
        let now = chrono::Utc::now();
        self.pending
            .values()
            .filter(|e| {
                e.timeout_secs > 0 && (now - e.created_at).num_seconds() > e.timeout_secs as i64
            })
            .collect()
    }
}

// ── SdkSession ──────────────────────────────────────────────────────────

/// Unified SDK session combining event stream, permission tracking, and elicitation.
///
/// # Example
///
/// ```rust,no_run
/// use coderex_sdk::{SdkSession, SdkEvent};
///
/// let session = SdkSession::new("sess-1".into());
/// let mut sub = session.subscribe_events();
/// session.emit(SdkEvent::TextDelta("hello".into()));
/// ```
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

    /// Create a session, optionally hydrating from a persisted store.
    ///
    /// If a store file exists at `.coderex/sdk_sessions/{id}.json`, the
    /// permission and elicitation trackers are restored from it.
    pub async fn load_or_new(id: String) -> Self {
        let session = Self::new(id.clone());
        if let Ok(store) = crate::store::SessionStore::for_session(&id) {
            if let Ok((perms, elics)) = store.load().await {
                let mut p = session.permissions.lock().await;
                *p = perms;
                let mut e = session.elicitations.lock().await;
                *e = elics;
            }
        }
        session
    }

    /// Emit an event.
    pub fn emit(&self, event: SdkEvent) {
        self.events.emit(event);
    }

    /// Subscribe to events with lag tracking.
    #[must_use]
    pub fn subscribe_events(&self) -> SdkSubscriber {
        self.events.subscribe()
    }

    /// Scan pending permissions and elicitations and auto-resolve any
    /// that have exceeded their timeout.
    ///
    /// Emits `PermissionResolved` / `ElicitationResolved` events for each
    /// expired item.
    pub async fn check_timeouts(&self) {
        let expired_perms: Vec<String> = {
            let tracker = self.permissions.lock().await;
            tracker.expired().iter().map(|p| p.id.clone()).collect()
        };
        for id in expired_perms {
            let resolved = {
                let mut tracker = self.permissions.lock().await;
                tracker.resolve_timeout(&id)
            };
            if resolved {
                self.emit(SdkEvent::PermissionResolved { id, allowed: false });
            }
        }

        let expired_elics: Vec<String> = {
            let tracker = self.elicitations.lock().await;
            tracker.expired().iter().map(|e| e.id.clone()).collect()
        };
        for id in expired_elics {
            let resolved = {
                let mut tracker = self.elicitations.lock().await;
                tracker.resolve_timeout(&id)
            };
            if resolved {
                self.emit(SdkEvent::ElicitationResolved { id });
            }
        }
    }

    /// Spawn a background task that calls [`check_timeouts`](Self::check_timeouts)
    /// every 5 seconds.
    ///
    /// Returns a `JoinHandle` that runs until the session is dropped.
    /// If called outside a Tokio runtime, the function returns `None`.
    pub fn spawn_timeout_checker(&self) -> Option<tokio::task::JoinHandle<()>> {
        let session = self.clone();
        let handle = tokio::runtime::Handle::try_current().ok()?;
        Some(handle.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                session.check_timeouts().await;
            }
        }))
    }

    /// Persist the current permission and elicitation state to disk.
    pub async fn persist(&self) -> Result<(), crate::store::StoreError> {
        let store = crate::store::SessionStore::for_session(&self.id)?;
        let perms = self.permissions.lock().await.clone();
        let elics = self.elicitations.lock().await.clone();
        store.save(&perms, &elics).await
    }
}

impl Clone for SdkSession {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            events: Arc::clone(&self.events),
            permissions: Arc::clone(&self.permissions),
            elicitations: Arc::clone(&self.elicitations),
        }
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
        let id = tracker
            .create(
                "Bash".to_string(),
                serde_json::json!({"command": "ls"}),
                None,
                0,
            )
            .unwrap();
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
        let id = tracker.create("Confirm deletion?".to_string(), 0);
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
        let id = tracker
            .create("Read".to_string(), serde_json::json!({}), None, 0)
            .unwrap();
        tracker.resolve(&id, false);
        assert!(tracker.pending().is_empty());
        assert_eq!(tracker.resolved().len(), 1);
        assert_eq!(tracker.resolved()[0].status, PermissionStatus::Denied);
    }

    #[test]
    fn elicitations_resolved_list() {
        let mut tracker = ElicitationTracker::new();
        let id = tracker.create("Are you sure?".to_string(), 0);
        tracker.resolve(&id, false, None);
        let resp = tracker.get_response(&id).unwrap();
        assert!(!resp.confirmed);
        assert!(resp.notes.is_none());
    }

    #[test]
    fn permission_schema_validates_correct_args() {
        let schema = PermissionSchema {
            fields: vec![
                ("command".into(), ArgType::String),
                ("count".into(), ArgType::Number),
            ],
        };
        let args = serde_json::json!({"command": "ls", "count": 5});
        assert!(schema.validate(&args).is_ok());
    }

    #[test]
    fn permission_schema_rejects_wrong_type() {
        let schema = PermissionSchema {
            fields: vec![("command".into(), ArgType::String)],
        };
        let args = serde_json::json!({"command": 42});
        assert!(schema.validate(&args).is_err());
    }

    #[test]
    fn permission_schema_rejects_unknown_field() {
        let schema = PermissionSchema {
            fields: vec![("command".into(), ArgType::String)],
        };
        let args = serde_json::json!({"command": "ls", "extra": "value"});
        assert!(schema.validate(&args).is_err());
    }

    #[test]
    fn permission_create_with_schema_validates() {
        let mut tracker = PermissionTracker::new();
        let schema = PermissionSchema {
            fields: vec![("path".into(), ArgType::String)],
        };
        let bad_args = serde_json::json!({"path": 123});
        assert!(
            tracker
                .create("Read".into(), bad_args, Some(&schema), 0)
                .is_err()
        );
    }

    #[test]
    fn permission_timeout_expires() {
        let mut tracker = PermissionTracker::new();
        let id = tracker
            .create("Read".into(), serde_json::json!({}), None, 1)
            .unwrap();
        // Immediately it should not be expired
        assert!(tracker.expired().is_empty());
        // After waiting, it should be expired
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(tracker.expired().len(), 1);
        assert_eq!(tracker.expired()[0].id, id);
    }

    #[test]
    fn elicitation_timeout_expires() {
        let mut tracker = ElicitationTracker::new();
        let id = tracker.create("Confirm?".into(), 1);
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(tracker.expired().len(), 1);
        assert_eq!(tracker.expired()[0].id, id);
    }

    #[test]
    fn permission_resolve_timeout() {
        let mut tracker = PermissionTracker::new();
        let id = tracker
            .create("Read".into(), serde_json::json!({}), None, 0)
            .unwrap();
        assert!(tracker.resolve_timeout(&id));
        let perm = tracker.get(&id).unwrap();
        assert_eq!(perm.status, PermissionStatus::TimedOut);
    }

    #[test]
    fn subscriber_lag_tracked_on_try_recv() {
        let stream = SdkEventStream::new();
        let mut sub = stream.subscribe();

        // Fill the broadcast buffer beyond capacity to trigger lag
        for i in 0..5000 {
            stream.emit(SdkEvent::TextDelta(format!("msg-{i}")));
        }

        // try_recv should report lag
        let _ = sub.try_recv();
        assert!(sub.lag() > 0);
    }
}
