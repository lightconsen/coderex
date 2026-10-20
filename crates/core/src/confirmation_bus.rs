//! Confirmation bus — decouples permission UI from tool execution.
//!
//! The [`ConfirmationBus`] provides an async channel for permission requests
//! and responses. When a tool requires user approval, the execution pipeline
//! sends a [`ConfirmationRequest`] through the bus and asynchronously awaits
//! a [`ConfirmationResponse`] from the UI layer.
//!
//! This decouples the engine's tool execution from any specific UI implementation,
//! enabling headless mode, server APIs, and IDE integrations to handle
//! confirmations in their own way.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};

/// A request for user confirmation to execute a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationRequest {
    /// Unique request ID.
    pub id: String,
    /// Tool name being requested.
    pub tool_name: String,
    /// JSON-serialized tool arguments.
    pub arguments: String,
    /// Optional path the tool will operate on.
    pub path: Option<std::path::PathBuf>,
    /// Human-readable reason for the confirmation.
    pub reason: String,
    /// Whether this is an "ask for session" request.
    pub allow_session: bool,
    /// Context metadata (session ID, turn number, etc.).
    pub context: HashMap<String, String>,
}

/// Response to a confirmation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfirmationResponse {
    /// The user approved this single operation.
    Approved,
    /// The user approved for the rest of the session.
    ApproveSession,
    /// The user denied the operation.
    Denied,
}

/// Internal message sent through the bus channel.
#[derive(Debug)]
struct BusMessage {
    request: ConfirmationRequest,
    /// One-shot channel to send the response back.
    responder: oneshot::Sender<ConfirmationResponse>,
}

/// A confirmation bus that decouples permission requests from responses.
///
/// The bus has two sides:
/// - **Requester side**: Tools/scheduler call `request()` to send a confirmation
///   request and await a response.
/// - **Responder side**: UI/repl calls `next_request()` to receive pending
///   requests and `respond()` to send back answers.
///
/// # Example
///
/// ```rust,ignore
/// use coderex_core::confirmation_bus::ConfirmationBus;
///
/// let bus = ConfirmationBus::new(10);
///
/// // In the tool scheduler (requester side)
/// tokio::spawn(async move {
///     let response = bus.request(confirmation_request).await;
///     // Proceed based on response
/// });
///
/// // In the UI (responder side)
/// tokio::spawn(async move {
///     if let Some(req) = bus.next_request().await {
///         // Show confirmation prompt to user
///         bus.respond(&req.id, ConfirmationResponse::Approved).await;
///     }
/// });
/// ```
#[derive(Debug, Clone)]
pub struct ConfirmationBus {
    /// Channel for sending requests from scheduler to UI.
    tx: mpsc::Sender<BusMessage>,
    /// Channel for receiving requests in the UI.
    rx: Arc<Mutex<mpsc::Receiver<BusMessage>>>,
    /// Store pending responders by request ID.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<ConfirmationResponse>>>>,
}

impl ConfirmationBus {
    /// Create a new confirmation bus with the given channel capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Send a confirmation request and await the response.
    ///
    /// This is called from the tool execution side. It blocks until the
    /// UI layer calls `respond()` for this request.
    ///
    /// Returns `None` if the bus is closed (no UI listening).
    pub async fn request(&self, req: ConfirmationRequest) -> Option<ConfirmationResponse> {
        let (responder_tx, responder_rx) = oneshot::channel();

        let msg = BusMessage {
            request: req,
            responder: responder_tx,
        };

        // Send the message through the bus
        if self.tx.send(msg).await.is_err() {
            return None;
        }

        // Wait for the response
        responder_rx.await.ok()
    }

    /// Receive the next pending confirmation request.
    ///
    /// This is called from the UI side. Returns `None` if the bus is closed.
    pub async fn next_request(&self) -> Option<ConfirmationRequest> {
        // Each guard is scoped so its lock is released before the next one is
        // taken: holding both would make a responder wait behind a receiver
        // that is itself waiting for a message.
        let msg = {
            let mut rx = self.rx.lock().await;
            rx.recv().await?
        };
        {
            // Store the responder so respond() can find it.
            let mut pending = self.pending.lock().await;
            pending.insert(msg.request.id.clone(), msg.responder);
        }
        Some(msg.request)
    }

    /// Respond to a pending confirmation request.
    ///
    /// This is called from the UI side after the user makes a decision.
    /// Returns `true` if the response was successfully delivered.
    pub async fn respond(&self, request_id: &str, response: ConfirmationResponse) -> bool {
        let mut pending = self.pending.lock().await;
        pending
            .remove(request_id)
            .is_some_and(|responder| responder.send(response).is_ok())
    }

    /// Check if there are pending requests waiting for a response.
    #[must_use]
    pub async fn has_pending(&self) -> bool {
        let pending = self.pending.lock().await;
        !pending.is_empty()
    }

    /// Count of pending requests.
    #[must_use]
    pub async fn pending_count(&self) -> usize {
        let pending = self.pending.lock().await;
        pending.len()
    }
}

/// A confirmation bus that auto-approves all requests (for testing/headless).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoApproveBus;

impl AutoApproveBus {
    /// Auto-approve all requests with the given response type.
    //
    // Kept `async` so it can stand in for `ConfirmationBus` at every call
    // site; there is no work to await.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn request(&self, _req: ConfirmationRequest) -> Option<ConfirmationResponse> {
        Some(ConfirmationResponse::Approved)
    }
}

/// A confirmation bus that auto-denies all requests (for testing/headless).
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoDenyBus;

impl AutoDenyBus {
    /// Auto-deny all requests.
    //
    // Kept `async` for the same reason as `AutoApproveBus`.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn request(&self, _req: ConfirmationRequest) -> Option<ConfirmationResponse> {
        Some(ConfirmationResponse::Denied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_and_respond_roundtrip() {
        let bus = ConfirmationBus::new(10);

        let bus_clone = bus.clone();
        let request = ConfirmationRequest {
            id: "req_1".into(),
            tool_name: "bash".into(),
            arguments: r#"{"command": "echo hi"}"#.into(),
            path: None,
            reason: "bash is dangerous".into(),
            allow_session: true,
            context: HashMap::new(),
        };

        let request_clone = request.clone();
        let handle = tokio::spawn(async move { bus_clone.request(request_clone).await });

        let received = bus.next_request().await.unwrap();
        assert_eq!(received.id, "req_1");
        assert_eq!(received.tool_name, "bash");

        bus.respond("req_1", ConfirmationResponse::Approved).await;

        let response = handle.await.unwrap().unwrap();
        assert_eq!(response, ConfirmationResponse::Approved);
    }

    #[tokio::test]
    async fn session_approval_response() {
        let bus = ConfirmationBus::new(10);

        let request = ConfirmationRequest {
            id: "req_2".into(),
            tool_name: "write".into(),
            arguments: r#"{"file_path": "/tmp/test.txt"}"#.into(),
            path: Some(std::path::PathBuf::from("/tmp/test.txt")),
            reason: "writing to file system".into(),
            allow_session: true,
            context: HashMap::new(),
        };

        let bus_clone = bus.clone();
        let handle = tokio::spawn(async move { bus_clone.request(request).await });

        let received = bus.next_request().await.unwrap();
        assert!(received.allow_session);

        bus.respond("req_2", ConfirmationResponse::ApproveSession)
            .await;

        let response = handle.await.unwrap().unwrap();
        assert_eq!(response, ConfirmationResponse::ApproveSession);
    }

    #[tokio::test]
    async fn deny_response() {
        let bus = ConfirmationBus::new(10);

        let request = ConfirmationRequest {
            id: "req_3".into(),
            tool_name: "rm".into(),
            arguments: r#"{"command": "rm -rf /"}"#.into(),
            path: None,
            reason: "destructive command".into(),
            allow_session: false,
            context: HashMap::new(),
        };

        let bus_clone = bus.clone();
        let handle = tokio::spawn(async move { bus_clone.request(request).await });

        let received = bus.next_request().await.unwrap();
        assert_eq!(received.tool_name, "rm");

        bus.respond("req_3", ConfirmationResponse::Denied).await;

        let response = handle.await.unwrap().unwrap();
        assert_eq!(response, ConfirmationResponse::Denied);
    }

    #[tokio::test]
    async fn multiple_requests_in_order() {
        let bus = ConfirmationBus::new(10);

        let requests = vec![
            ConfirmationRequest {
                id: "req_a".into(),
                tool_name: "read".into(),
                arguments: "{}".into(),
                path: None,
                reason: "reading file".into(),
                allow_session: false,
                context: HashMap::new(),
            },
            ConfirmationRequest {
                id: "req_b".into(),
                tool_name: "write".into(),
                arguments: "{}".into(),
                path: None,
                reason: "writing file".into(),
                allow_session: false,
                context: HashMap::new(),
            },
        ];

        let bus_clone = bus.clone();
        let requests_clone = requests.clone();
        let handle = tokio::spawn(async move {
            let mut responses = Vec::new();
            for req in requests_clone {
                responses.push(bus_clone.request(req).await);
            }
            responses
        });

        let req1 = bus.next_request().await.unwrap();
        assert_eq!(req1.id, "req_a");
        bus.respond("req_a", ConfirmationResponse::Approved).await;

        let req2 = bus.next_request().await.unwrap();
        assert_eq!(req2.id, "req_b");
        bus.respond("req_b", ConfirmationResponse::Denied).await;

        let responses = handle.await.unwrap();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0], Some(ConfirmationResponse::Approved));
        assert_eq!(responses[1], Some(ConfirmationResponse::Denied));
    }
}
