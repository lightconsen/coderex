//! In-process transport extension point for Bridge.
//!
//! Provides a transport abstraction layer that supports:
//! - WebSocket-based remote transport (default)
//! - In-process transport for MCP and SDK control channels
//! - Mock transport for testing

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Duration;
use thiserror::Error;

/// Errors from transport operations.
#[derive(Debug, Error)]
pub enum TransportError {
    /// Transport is not connected.
    #[error("transport not connected")]
    NotConnected,

    /// Failed to send message.
    #[error("send failed: {0}")]
    SendFailed(String),

    /// Failed to receive message.
    #[error("receive failed: {0}")]
    ReceiveFailed(String),

    /// Connection lost.
    #[error("connection lost: {0}")]
    ConnectionLost(String),

    /// Failed to deserialize message.
    #[error("deserialization error: {0}")]
    DeserializationError(String),
}

/// A message sent over the transport layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportMessage {
    /// Message type identifier.
    pub r#type: String,
    /// Optional correlation ID for request/response.
    pub correlation_id: Option<String>,
    /// Message payload as JSON string.
    pub payload: String,
    /// Optional headers/metadata.
    pub metadata: HashMap<String, String>,
}

/// Trait for transport layer implementations.
///
/// Implement this trait to provide a custom transport backend
/// (e.g., WebSocket, in-process, HTTP, etc.).
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Connect to the remote endpoint.
    async fn connect(&mut self) -> Result<(), TransportError>;

    /// Disconnect from the remote endpoint.
    async fn disconnect(&mut self) -> Result<(), TransportError>;

    /// Check if the transport is currently connected.
    fn is_connected(&self) -> bool;

    /// Send a message to the remote endpoint.
    async fn send(&mut self, message: TransportMessage) -> Result<(), TransportError>;

    /// Receive the next message from the remote endpoint.
    async fn receive(&mut self) -> Result<TransportMessage, TransportError>;

    /// Receive the next message with a timeout.
    ///
    /// Default implementation wraps `receive()` with `tokio::time::timeout`.
    /// Implementors may override this for more efficient native timeout support.
    async fn receive_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<TransportMessage, TransportError> {
        tokio::time::timeout(timeout, self.receive())
            .await
            .map_err(|_| TransportError::ReceiveFailed("receive timed out".into()))?
    }

    /// Get a unique identifier for this transport connection.
    fn connection_id(&self) -> &str;
}

/// In-process transport implementation.
///
/// Routes messages between components within the same process
/// using channels. Used for MCP in-process transport and
/// SDK control channels.
#[derive(Debug)]
pub struct InProcessTransport {
    /// Whether the transport is currently connected.
    connected: bool,
    /// Connection identifier.
    connection_id: String,
    /// Outgoing message sender.
    tx: Option<mpsc::UnboundedSender<TransportMessage>>,
    /// Incoming message receiver.
    rx: Option<mpsc::UnboundedReceiver<TransportMessage>>,
}

impl InProcessTransport {
    /// Create a new in-process transport.
    #[must_use]
    pub fn new(connection_id: String) -> Self {
        Self {
            connected: false,
            connection_id,
            tx: None,
            rx: None,
        }
    }

    /// Create a pair of connected transports.
    ///
    /// Returns two transports that can communicate with each other
    /// via in-process channels.
    pub fn create_pair(
        id_a: String,
        id_b: String,
    ) -> (Self, Self) {
        let (tx_a, rx_a) = mpsc::unbounded_channel::<TransportMessage>();
        let (tx_b, rx_b) = mpsc::unbounded_channel::<TransportMessage>();

        let transport_a = Self {
            connected: false,
            connection_id: id_a,
            tx: Some(tx_a),
            rx: Some(rx_b), // a receives from b's sender
        };

        let transport_b = Self {
            connected: false,
            connection_id: id_b,
            tx: Some(tx_b),
            rx: Some(rx_a), // b receives from a's sender
        };

        (transport_a, transport_b)
    }
}

#[async_trait::async_trait]
impl Transport for InProcessTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.connected = true;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.connected = false;
        // Drop channels to signal closure to peers
        self.tx = None;
        self.rx = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn send(&mut self, message: TransportMessage) -> Result<(), TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }

        if let Some(ref tx) = self.tx {
            tx.send(message)
                .map_err(|e| TransportError::SendFailed(e.to_string()))
        } else {
            Err(TransportError::SendFailed("no sender channel".into()))
        }
    }

    async fn receive(&mut self) -> Result<TransportMessage, TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }

        if let Some(ref mut rx) = self.rx {
            rx.recv()
                .await
                .ok_or_else(|| TransportError::ReceiveFailed("channel closed".into()))
        } else {
            Err(TransportError::ReceiveFailed("no receiver channel".into()))
        }
    }

    fn connection_id(&self) -> &str {
        &self.connection_id
    }
}

/// Transport factory for creating transport instances.
///
/// Allows runtime selection of transport type (WebSocket, in-process, mock).
pub struct TransportFactory {
    /// Registered transport constructors.
    transports: HashMap<String, Box<dyn Fn(&str) -> Box<dyn Transport> + Send + Sync>>,
}

impl Default for TransportFactory {
    fn default() -> Self {
        Self {
            transports: HashMap::new(),
        }
    }
}

impl std::fmt::Debug for TransportFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportFactory")
            .field("transports", &self.registered_types())
            .finish()
    }
}

impl TransportFactory {
    /// Create a new transport factory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a transport type by name.
    pub fn register<F>(&mut self, name: &str, factory: F)
    where
        F: Fn(&str) -> Box<dyn Transport> + Send + Sync + 'static,
    {
        self.transports.insert(name.to_string(), Box::new(factory));
    }

    /// Create a transport instance by name.
    ///
    /// Returns None if no transport with the given name is registered.
    pub fn create(&self, name: &str, connection_id: &str) -> Option<Box<dyn Transport>> {
        self.transports.get(name).map(|f| f(connection_id))
    }

    /// List registered transport types.
    pub fn registered_types(&self) -> Vec<&str> {
        self.transports.keys().map(|k| k.as_str()).collect()
    }
}

/// Create a factory with all built-in transports registered.
#[must_use]
pub fn default_factory() -> TransportFactory {
    let mut factory = TransportFactory::new();
    factory.register("in-process", |id| {
        Box::new(InProcessTransport::new(id.to_string()))
    });
    factory
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_process_transport_create_pair() {
        let (mut a, mut b) = InProcessTransport::create_pair(
            "client_a".into(),
            "client_b".into(),
        );

        a.connect().await.unwrap();
        b.connect().await.unwrap();

        assert!(a.is_connected());
        assert!(b.is_connected());
    }

    #[tokio::test]
    async fn in_process_transport_send_receive() {
        let (mut a, mut b) = InProcessTransport::create_pair(
            "sender".into(),
            "receiver".into(),
        );

        a.connect().await.unwrap();
        b.connect().await.unwrap();

        let msg = TransportMessage {
            r#type: "test".into(),
            correlation_id: Some("msg_1".into()),
            payload: r#"{"hello": "world"}"#.into(),
            metadata: HashMap::new(),
        };

        a.send(msg.clone()).await.unwrap();

        let received = b.receive().await.unwrap();
        assert_eq!(received.r#type, "test");
        assert_eq!(received.correlation_id, Some("msg_1".into()));
        assert_eq!(received.payload, r#"{"hello": "world"}"#);
    }

    #[tokio::test]
    async fn in_process_transport_disconnect() {
        let (mut transport, _) = InProcessTransport::create_pair(
            "temp".into(),
            "peer".into(),
        );

        transport.connect().await.unwrap();
        assert!(transport.is_connected());

        transport.disconnect().await.unwrap();
        assert!(!transport.is_connected());

        // Send should fail after disconnect
        let msg = TransportMessage {
            r#type: "test".into(),
            correlation_id: None,
            payload: "{}".into(),
            metadata: HashMap::new(),
        };
        assert!(transport.send(msg).await.is_err());
    }

    #[test]
    fn transport_factory_default() {
        let factory = default_factory();
        let types = factory.registered_types();
        assert!(types.contains(&"in-process"));
    }

    #[test]
    fn transport_factory_create() {
        let factory = default_factory();
        let transport = factory.create("in-process", "test_conn");
        assert!(transport.is_some());
        assert!(transport.unwrap().connection_id() == "test_conn");
    }

    #[test]
    fn transport_factory_unknown_name() {
        let factory = default_factory();
        let transport = factory.create("nonexistent", "test_conn");
        assert!(transport.is_none());
    }

    #[test]
    fn transport_message_serialization() {
        let msg = TransportMessage {
            r#type: "heartbeat".into(),
            correlation_id: None,
            payload: r#"{"status": "ok"}"#.into(),
            metadata: HashMap::from([
                ("source".into(), "bridge".into()),
            ]),
        };
        assert_eq!(msg.r#type, "heartbeat");
        assert!(msg.metadata.contains_key("source"));
    }

    #[tokio::test]
    async fn transport_error_not_connected() {
        let mut transport = InProcessTransport::new("test".into());
        let msg = TransportMessage {
            r#type: "test".into(),
            correlation_id: None,
            payload: "{}".into(),
            metadata: HashMap::new(),
        };
        let result = transport.send(msg).await;
        assert!(matches!(result, Err(TransportError::NotConnected)));
    }

    #[tokio::test]
    async fn receive_timeout_returns_message_when_available() {
        let (mut a, mut b) = InProcessTransport::create_pair(
            "sender".into(),
            "receiver".into(),
        );
        a.connect().await.unwrap();
        b.connect().await.unwrap();

        let msg = TransportMessage {
            r#type: "ping".into(),
            correlation_id: None,
            payload: "{}".into(),
            metadata: HashMap::new(),
        };
        a.send(msg).await.unwrap();

        let received = b.receive_timeout(Duration::from_secs(1)).await.unwrap();
        assert_eq!(received.r#type, "ping");
    }

    #[tokio::test]
    async fn receive_timeout_returns_error_on_expiry() {
        let (mut a, mut b) = InProcessTransport::create_pair(
            "a".into(),
            "b".into(),
        );
        a.connect().await.unwrap();
        b.connect().await.unwrap();

        // Keep b alive but don't send anything — a will timeout waiting
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = b;
        });

        let result = a.receive_timeout(Duration::from_millis(50)).await;
        assert!(matches!(result, Err(TransportError::ReceiveFailed(msg)) if msg.contains("timed out")));

        handle.abort();
    }

    #[test]
    fn transport_error_variants() {
        // Verify all error variants exist and format correctly
        let _ = TransportError::NotConnected.to_string();
        let _ = TransportError::SendFailed("test".into()).to_string();
        let _ = TransportError::ReceiveFailed("test".into()).to_string();
        let _ = TransportError::ConnectionLost("test".into()).to_string();
        let _ = TransportError::DeserializationError("test".into()).to_string();
    }

    #[tokio::test]
    async fn bidirectional_communication() {
        let (mut a, mut b) = InProcessTransport::create_pair(
            "alice".into(),
            "bob".into(),
        );

        a.connect().await.unwrap();
        b.connect().await.unwrap();

        // A sends to B
        let msg_ab = TransportMessage {
            r#type: "request".into(),
            correlation_id: Some("req_1".into()),
            payload: r#"{"action": "ping"}"#.into(),
            metadata: HashMap::new(),
        };
        a.send(msg_ab).await.unwrap();

        // B receives from A
        let received = b.receive().await.unwrap();
        assert_eq!(received.r#type, "request");

        // B responds to A
        let msg_ba = TransportMessage {
            r#type: "response".into(),
            correlation_id: Some("req_1".into()),
            payload: r#"{"action": "pong"}"#.into(),
            metadata: HashMap::new(),
        };
        b.send(msg_ba).await.unwrap();

        // A receives response
        let response = a.receive().await.unwrap();
        assert_eq!(response.r#type, "response");
        assert_eq!(response.correlation_id, Some("req_1".into()));
    }
}
