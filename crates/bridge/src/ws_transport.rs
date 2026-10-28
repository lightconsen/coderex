//! WebSocket transport implementation for the Bridge.
//!
//! Provides a WebSocket client that implements the [`Transport`] trait,
//! supporting TLS via `tokio-tungstenite` and token-based authentication
//! via the `Authorization` header.

use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::marker::PhantomPinned;
use tokio::time::Duration;
use tokio_tungstenite::Connector;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, info, warn};

use crate::transport::{Transport, TransportError, TransportMessage};

/// Configuration for a WebSocket transport.
#[derive(Debug, Clone)]
pub struct WsTransportConfig {
    /// WebSocket URL (`wss://` or `ws://`).
    pub url: String,
    /// Authentication token (sent in `Authorization` header).
    pub auth_token: Option<String>,
    /// Connection ID label.
    pub connection_id: String,
}

impl WsTransportConfig {
    /// Create a new config with the given URL.
    pub fn new(url: impl Into<String>) -> Self {
        let url_str = url.into();
        Self {
            connection_id: url_str.clone(),
            url: url_str,
            auth_token: None,
        }
    }

    /// Set the authentication token.
    #[must_use]
    pub fn with_auth(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }
}

/// WebSocket-based transport implementation.
///
/// Connects to a remote WebSocket endpoint with optional TLS and
/// token-based authentication. Messages are bridged between the
/// WebSocket stream and internal channels for the [`Transport`] trait.
pub struct WebSocketTransport {
    /// Connection identifier.
    connection_id: String,
    /// Configuration used for the initial connection and for reconnects.
    config: WsTransportConfig,
    /// Whether currently connected.
    connected: bool,
    /// Receiver from the read loop.
    rx: tokio::sync::mpsc::UnboundedReceiver<TransportMessage>,
    /// Sender from the write loop (internal -> WS).
    write_tx: tokio::sync::mpsc::UnboundedSender<TransportMessage>,
    /// Handle to the spawned reader task.
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Handle to the spawned writer task.
    writer_handle: Option<tokio::task::JoinHandle<()>>,
    /// Keep self !Unpinned for safety.
    _pinned: PhantomPinned,
}

impl std::fmt::Debug for WebSocketTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketTransport")
            .field("connection_id", &self.connection_id)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

impl WebSocketTransport {
    /// Create a new WebSocket transport from the given configuration.
    #[must_use]
    pub fn new(config: WsTransportConfig) -> Self {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel::<TransportMessage>();
        let (write_tx, _) = tokio::sync::mpsc::unbounded_channel::<TransportMessage>();

        Self {
            connection_id: config.connection_id.clone(),
            config,
            connected: false,
            rx,
            write_tx,
            reader_handle: None,
            writer_handle: None,
            _pinned: PhantomPinned,
        }
    }

    /// Connect to the remote WebSocket endpoint.
    pub async fn connect_ws(&mut self, config: WsTransportConfig) -> Result<(), TransportError> {
        let url = config.url.clone();
        // Keep the configuration so `Transport::connect` can re-establish the
        // same endpoint (with the same credentials) after a stream ends.
        self.config = config.clone();
        self.connection_id.clone_from(&config.connection_id);

        // Build request with auth header
        let mut request = url
            .clone()
            .into_client_request()
            .map_err(|e| TransportError::ConnectionLost(format!("invalid URL: {e}")))?;

        if let Some(ref token) = config.auth_token {
            let auth_value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| TransportError::ConnectionLost(format!("invalid auth token: {e}")))?;
            request.headers_mut().insert("Authorization", auth_value);
        }

        // TLS connector: Plain for ws://, default rustls for wss://
        let connector = if url.starts_with("wss://") {
            None // use default webpki-roots
        } else {
            Some(Connector::Plain)
        };

        let (ws_stream, _response) = connect_async_tls_with_config(request, None, true, connector)
            .await
            .map_err(|e| TransportError::ConnectionLost(format!("connection failed: {e}")))?;

        info!("WebSocket connected to {url}");

        let (write_send, write_recv) = tokio::sync::mpsc::unbounded_channel::<TransportMessage>();
        self.write_tx = write_send;

        // The reader owns the only sender: keeping one here would keep the
        // channel open after the socket closed, so `receive()` would block
        // forever and the orchestrator's reconnect path would never run.
        let (tx_inner, rx_inner) = tokio::sync::mpsc::unbounded_channel::<TransportMessage>();
        self.rx = rx_inner;

        // Split the stream
        let (mut ws_write, mut ws_read) = ws_stream.split();

        // Spawn reader task: WS -> internal channel
        let reader_tx = tx_inner;
        let reader_handle = tokio::spawn(async move {
            while let Some(msg) = ws_read.next().await {
                match msg {
                    Ok(WsMessage::Text(text)) => {
                        debug!("WS received text: {} chars", text.len());
                        match serde_json::from_str::<TransportMessage>(&text) {
                            Ok(tm) => {
                                let _ = reader_tx.send(tm);
                            }
                            Err(e) => {
                                warn!("failed to parse transport message: {e}");
                            }
                        }
                    }
                    Ok(WsMessage::Close(_)) => {
                        debug!("WebSocket closed");
                        break;
                    }
                    Ok(WsMessage::Ping(_)) => {
                        // pong is handled automatically by tungstenite
                    }
                    Ok(other) => {
                        debug!("WS received non-text message: {other:?}");
                    }
                    Err(e) => {
                        warn!("WebSocket read error: {e}");
                        break;
                    }
                }
            }
        });

        // Spawn writer task: internal channel -> WS
        let writer_handle = tokio::spawn(async move {
            let mut recv = write_recv;
            while let Some(msg) = recv.recv().await {
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        warn!("failed to serialize transport message: {e}");
                        continue;
                    }
                };
                if let Err(e) = ws_write.send(WsMessage::Text(json.into())).await {
                    warn!("WebSocket send error: {e}");
                    break;
                }
            }
        });

        self.reader_handle = Some(reader_handle);
        self.writer_handle = Some(writer_handle);
        self.connected = true;

        Ok(())
    }
}

#[async_trait::async_trait]
impl Transport for WebSocketTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        let config = self.config.clone();
        self.connect_ws(config).await
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        if let Some(handle) = self.writer_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn send(&mut self, message: TransportMessage) -> Result<(), TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        self.write_tx
            .send(message)
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }

    async fn receive(&mut self) -> Result<TransportMessage, TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        self.rx
            .recv()
            .await
            .ok_or_else(|| TransportError::ReceiveFailed("channel closed".into()))
    }

    async fn receive_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<TransportMessage, TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        tokio::time::timeout(timeout, self.receive())
            .await
            .map_err(|_| TransportError::ReceiveFailed("receive timed out".into()))?
    }

    fn connection_id(&self) -> &str {
        &self.connection_id
    }
}

/// Store and manage bridge authentication tokens.
///
/// Tokens are stored in `~/.coderex/bridge_tokens.json` as a list of
/// named token entries. Provides CRUD operations for token management.
#[derive(Debug, Default, Clone)]
pub struct BridgeTokenStore {
    tokens: HashMap<String, String>, // name -> token
}

impl BridgeTokenStore {
    /// Default path for the token store.
    pub fn default_path() -> std::path::PathBuf {
        let mut path = home::home_dir().unwrap_or_default();
        path.push(".coderex/bridge_tokens.json");
        path
    }

    /// Load tokens from the default path.
    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    /// Load tokens from a specific path.
    pub fn load_from(path: &std::path::Path) -> Self {
        if path.exists()
            && let Ok(content) = std::fs::read_to_string(path)
            && let Ok(tokens) = serde_json::from_str::<HashMap<String, String>>(&content)
        {
            return Self { tokens };
        }
        Self::default()
    }

    /// Save tokens to the default path.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::default_path())
    }

    /// Save tokens to a specific path.
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&self.tokens).map_err(std::io::Error::other)?;
        std::fs::write(path, content)
    }

    /// Add or update a token.
    pub fn upsert(&mut self, name: &str, token: &str) {
        self.tokens.insert(name.to_string(), token.to_string());
    }

    /// Remove a token.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.tokens.remove(name)
    }

    /// Check if a token is valid.
    #[must_use]
    pub fn validate(&self, token: &str) -> bool {
        self.tokens.values().any(|t| t == token)
    }

    /// List token names.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tokens
            .keys()
            .map(std::string::String::as_str)
            .collect()
    }

    /// Number of tokens.
    #[must_use]
    pub fn count(&self) -> usize {
        self.tokens.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_transport_config_builder() {
        let config = WsTransportConfig::new("wss://example.com/ws").with_auth("secret-token");
        assert_eq!(config.url, "wss://example.com/ws");
        assert_eq!(config.auth_token, Some("secret-token".into()));
    }

    #[test]
    fn ws_transport_not_connected_initially() {
        let config = WsTransportConfig::new("wss://localhost:8080/ws");
        let transport = WebSocketTransport::new(config);
        assert!(!transport.is_connected());
    }

    #[test]
    fn ws_transport_disconnect_without_connect_is_ok() {
        let config = WsTransportConfig::new("wss://localhost:8080/ws");
        let mut transport = WebSocketTransport::new(config);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(transport.disconnect()).unwrap();
    }

    #[tokio::test]
    async fn connect_uses_the_configuration_given_at_construction() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = tokio_tungstenite::accept_async(stream).await;
        });

        let config = WsTransportConfig::new(format!("ws://{addr}")).with_auth("token");
        let mut transport = WebSocketTransport::new(config);

        transport.connect().await.unwrap();
        assert!(transport.is_connected());

        server.abort();
        transport.disconnect().await.unwrap();
    }

    #[test]
    fn ws_transport_send_fails_when_not_connected() {
        let config = WsTransportConfig::new("wss://localhost:8080/ws");
        let mut transport = WebSocketTransport::new(config);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let msg = TransportMessage {
            r#type: "test".into(),
            correlation_id: None,
            payload: "{}".into(),
            metadata: HashMap::new(),
        };
        let result = rt.block_on(transport.send(msg));
        assert!(matches!(result, Err(TransportError::NotConnected)));
    }

    #[test]
    fn token_store_upsert_and_validate() {
        let mut store = BridgeTokenStore::default();
        assert!(store.count() == 0);

        store.upsert("prod", "tok_abc123");
        store.upsert("dev", "tok_dev456");
        assert_eq!(store.count(), 2);

        assert!(store.validate("tok_abc123"));
        assert!(store.validate("tok_dev456"));
        assert!(!store.validate("invalid"));

        let names = store.names();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn token_store_remove() {
        let mut store = BridgeTokenStore::default();
        store.upsert("prod", "tok_abc");
        assert!(store.validate("tok_abc"));

        store.remove("prod");
        assert!(!store.validate("tok_abc"));
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn token_store_roundtrip_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tokens.json");

        let mut store = BridgeTokenStore::default();
        store.upsert("test", "secret");
        store.save_to(&path).unwrap();

        let loaded = BridgeTokenStore::load_from(&path);
        assert!(loaded.validate("secret"));
        assert_eq!(loaded.count(), 1);
    }
}
