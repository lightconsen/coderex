//! Detached WebSocket session management with authentication, rate limiting,
//! and idle cleanup.

use coderex_core::engine::{QueryEngine, SubmitOptions};
use coderex_core::types::EngineEvent;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// ─── Structured JSON Protocol Types ──────────────────────────────────────

/// Client → Server request message.
#[derive(Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct WsRequest {
    /// Unique request ID for correlation.
    pub id: String,
    /// Request type.
    #[serde(flatten)]
    pub kind: WsRequestKind,
}

/// Type of client request.
#[derive(Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsRequestKind {
    /// Send a query/prompt to the assistant.
    Query { content: String },
    /// Interrupt the current query.
    Interrupt,
    /// Ping to keep the connection alive.
    Ping,
}

/// Server → Client response message.
#[derive(Debug, Serialize)]
#[allow(missing_docs)]
pub struct WsResponse {
    /// Correlates to the request ID.
    pub id: String,
    /// Response payload.
    #[serde(flatten)]
    pub kind: WsResponseKind,
}

/// Type of server response.
#[derive(Debug, Serialize)]
#[allow(missing_docs)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsResponseKind {
    /// Successful query response.
    QueryOk { content: String },
    /// Streaming chunk from a query in progress.
    StreamChunk { chunk: String },
    /// Query completed, and why it ended.
    QueryDone {
        /// How the turn finished: `completed`, `token_limit_stop`, and so on.
        /// A truncated or interrupted answer is not the same as a finished one.
        reason: String,
    },
    /// Ping response.
    Pong,
    /// Error response.
    Error { code: String, message: String },
}

/// Server → Client unsolicited event (not correlated to a request).
#[derive(Debug, Serialize)]
#[allow(missing_docs)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WsEvent {
    /// Session is ready to receive messages.
    SessionReady { session_id: String },
    /// Warning that the session is approaching idle timeout.
    IdleWarning { message: String },
    /// Session has been closed.
    SessionClosed { reason: String },
    /// Rate limit exceeded.
    RateLimited,
}

impl WsResponse {
    fn error(id: &str, code: &str, message: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: WsResponseKind::Error {
                code: code.to_string(),
                message: message.to_string(),
            },
        }
    }

    fn pong(id: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: WsResponseKind::Pong,
        }
    }
}

/// Token-bucket rate limiter per session.
#[derive(Debug)]
struct TokenBucket {
    /// Maximum tokens.
    capacity: u32,
    /// Current tokens available.
    tokens: f64,
    /// Tokens added per second.
    refill_rate: f64,
    /// Last time tokens were refilled.
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        self.last_refill = now;
    }

    /// Try to consume one token. Returns false if rate limit exceeded.
    fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A connected WebSocket session.
#[derive(Debug, Clone)]
pub struct Session {
    /// The unique session identifier.
    pub id: String,
    /// The API key or token used for authentication.
    pub auth_token: String,
    /// When the session was created.
    pub created_at: Instant,
    /// Last activity timestamp.
    pub last_activity: Instant,
    /// Whether the session is authenticated.
    pub authenticated: bool,
}

/// Configuration for the daemon server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Valid API keys for authentication.
    pub valid_keys: Vec<String>,
    /// Serve without any API keys.
    ///
    /// Off by default: with no keys configured, an open server accepts any
    /// token from anyone who can reach the port. Turning this on is an
    /// explicit decision to run unauthenticated, and is meant for a loopback
    /// development session rather than anything reachable from a network.
    pub allow_anonymous: bool,
    /// Duration after which idle sessions are cleaned up.
    pub idle_timeout: Duration,
    /// Maximum number of concurrent sessions.
    pub max_sessions: usize,
    /// Maximum messages per minute per session (rate limit).
    pub rate_limit_per_minute: u32,
    /// Optional port for the HTTP health endpoint (None = disabled).
    pub health_port: Option<u16>,
    /// Optional path to SQLite database for session persistence.
    pub persist_path: Option<std::path::PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            valid_keys: Vec::new(),
            allow_anonymous: false,
            idle_timeout: Duration::from_secs(3600),
            max_sessions: 100,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        }
    }
}

/// Builds the engine a session's queries run on.
///
/// The server cannot build one itself — an engine needs an API client, a
/// permission checker, and a tool dispatcher, and those belong to whatever
/// started the server. The factory is how they are handed over, and it also
/// returns the event stream, because whoever builds the engine is who can read
/// what it emits.
pub type EngineFactory = Arc<
    dyn Fn() -> (
            QueryEngine,
            tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
        ) + Send
        + Sync,
>;

/// The state every connection shares.
///
/// Behind a synchronous lock on purpose: it is only ever held for the short
/// bookkeeping a message needs, never across an `await`, and a guard that is
/// not `Send` makes the compiler enforce that in the tasks that serve
/// connections.
#[derive(Debug, Default)]
struct Shared {
    /// Active sessions.
    sessions: HashMap<String, Session>,
    /// Optional SQLite session store for persistence.
    store: Option<crate::persistence::SessionStore>,
    /// Monotonic source of session ids.
    next_session_id: u64,
}

impl Shared {
    /// Open a session for a token that is allowed to have one.
    fn authenticate(&mut self, token: &str, config: &ServerConfig) -> Result<String, String> {
        if config.valid_keys.is_empty() {
            if !config.allow_anonymous {
                return Err(
                    "server has no API keys configured; set one, or enable anonymous \
                     access explicitly for local development"
                        .to_string(),
                );
            }
        } else if !config.valid_keys.iter().any(|k| k == token) {
            return Err("invalid API key".to_string());
        }
        if self.sessions.len() >= config.max_sessions {
            return Err("max sessions reached".to_string());
        }

        // A counter, not `sessions.len()`: removing a session lowers the
        // length, so the next id would repeat one still in the map and
        // overwrite it.
        self.next_session_id += 1;
        let id = format!("sess_{}", self.next_session_id);
        let now = Instant::now();
        let session = Session {
            id: id.clone(),
            auth_token: token.to_string(),
            created_at: now,
            last_activity: now,
            authenticated: true,
        };
        self.sessions.insert(id.clone(), session);

        // Persist to SQLite if configured. What reaches the file is a
        // fingerprint of the key, not the key: the row only has to say which
        // key opened the session.
        if let Some(ref mut store) = self.store {
            let session = self.sessions.get(&id).expect("just inserted");
            let fingerprint = store.fingerprint(&session.auth_token);
            let ps = crate::persistence::persist_session(session, fingerprint);
            if let Err(e) = store.save(&ps) {
                warn!("Failed to persist session {id}: {e}");
            }
        }

        Ok(id)
    }
}

impl Shared {
    /// Note that a session was used, so idle cleanup leaves it alone.
    fn record_activity(&mut self, session_id: &str) -> bool {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.last_activity = Instant::now();
            true
        } else {
            false
        }
    }

    /// Drop the sessions that have been idle past the timeout.
    fn cleanup_idle(&mut self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let mut removed = Vec::new();

        self.sessions.retain(|id, session| {
            if now.duration_since(session.last_activity) > timeout {
                removed.push(id.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// End a session, in memory and in the store.
    fn disconnect(&mut self, session_id: &str) -> bool {
        let removed = self.sessions.remove(session_id).is_some();
        if removed {
            if let Some(ref mut store) = self.store {
                if let Err(e) = store.remove(session_id) {
                    warn!("Failed to remove session {session_id} from store: {e}");
                }
            }
        }
        removed
    }
}

/// One WebSocket connection, with what it shares and what it owns.
struct Connection {
    config: ServerConfig,
    shared: Arc<parking_lot::Mutex<Shared>>,
    session_count_atomic: Arc<AtomicUsize>,
    engine_factory: Option<EngineFactory>,
}

impl Connection {
    fn authenticate(&self, token: &str) -> Result<String, String> {
        let id = self.shared.lock().authenticate(token, &self.config)?;
        self.sync_session_count();
        Ok(id)
    }

    fn record_activity(&self, session_id: &str) -> bool {
        self.shared.lock().record_activity(session_id)
    }

    fn disconnect(&self, session_id: &str) -> bool {
        let removed = self.shared.lock().disconnect(session_id);
        if removed {
            self.sync_session_count();
        }
        removed
    }

    fn session_count(&self) -> usize {
        self.shared.lock().sessions.len()
    }

    fn sync_session_count(&self) {
        self.session_count_atomic
            .store(self.shared.lock().sessions.len(), Ordering::Relaxed);
    }

    async fn serve(&self, stream: TcpStream) {
        let ws_result = tokio_tungstenite::accept_async(stream).await;
        let mut ws = match ws_result {
            Ok(ws) => ws,
            Err(e) => {
                warn!("WebSocket handshake failed: {e}");
                return;
            }
        };

        // First message must be auth token
        let auth_msg = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t,
            Ok(Some(Ok(other))) => {
                warn!("expected auth text, got: {other:?}");
                let _ = ws
                    .send(Message::Text(
                        r#"{"error":"auth required"}"#.to_string().into(),
                    ))
                    .await;
                return;
            }
            Ok(Some(Err(e))) => {
                warn!("WebSocket error during auth: {e}");
                return;
            }
            Ok(None) => {
                debug!("client disconnected before auth");
                return;
            }
            Err(_) => {
                warn!("auth timeout");
                let _ = ws
                    .send(Message::Text(
                        r#"{"error":"auth timeout"}"#.to_string().into(),
                    ))
                    .await;
                return;
            }
        };

        // Parse auth JSON: {"token":"..."}
        let token = match serde_json::from_str::<serde_json::Value>(&auth_msg) {
            Ok(v) => v
                .get("token")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => auth_msg.to_string(), // fallback: raw token
        };

        let session_id = match self.authenticate(&token) {
            Ok(id) => id,
            Err(e) => {
                warn!("authentication failed: {e}");
                let _ = ws
                    .send(Message::Text(format!(r#"{{"error":"{e}"}}"#).into()))
                    .await;
                return;
            }
        };

        let _ = ws
            .send(Message::Text(
                format!(r#"{{"session_id":"{session_id}"}}"#).into(),
            ))
            .await;

        info!(
            "session {session_id} authenticated, {} active",
            self.session_count()
        );

        // Per-connection rate limiter
        let refill_rate = self.config.rate_limit_per_minute as f64 / 60.0;
        let mut limiter = TokenBucket::new(self.config.rate_limit_per_minute, refill_rate);

        // Send session-ready event
        let ready = WsEvent::SessionReady {
            session_id: session_id.clone(),
        };
        let _ = ws
            .send(Message::Text(serde_json::to_string(&ready).unwrap().into()))
            .await;

        // Message loop with structured JSON protocol.
        //
        // Outgoing frames go through a channel so a query running in its own
        // task can stream while this loop keeps reading: the socket has one
        // writer, and it is here.
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // The queries this connection has running, so Ctrl+C and a disconnect
        // can reach them. A session belongs to one connection, so this is
        // per-connection state rather than shared.
        let mut running_queries: HashMap<String, coderex_core::engine::InterruptHandle> =
            HashMap::new();

        loop {
            tokio::select! {
                // A frame from a query running in its own task.
                Some(frame) = out_rx.recv() => {
                    if let Err(e) = ws.send(Message::Text(frame.into())).await {
                        warn!("send error for session {session_id}: {e}");
                        break;
                    }
                }
                incoming = tokio::time::timeout(Duration::from_secs(300), ws.next()) => {
                    match incoming {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            // Rate limit check
                            if !limiter.try_consume() {
                                let evt = WsEvent::RateLimited;
                                let _ = ws
                                    .send(Message::Text(
                                        serde_json::to_string(&evt).unwrap().into(),
                                    ))
                                    .await;
                                continue;
                            }
                            self.record_activity(&session_id);
                            debug!("session {session_id} received: {text}");

                            // A query answers through the channel as it runs;
                            // everything else answers here.
                            let Some(response) =
                                self.handle_message(&session_id, &text, &out_tx, &mut running_queries)
                            else {
                                continue;
                            };
                            let json = serde_json::to_string(&response).unwrap();
                            if let Err(e) = ws.send(Message::Text(json.into())).await {
                                warn!("send error for session {session_id}: {e}");
                                break;
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) => {
                            debug!("session {session_id} closed connection");
                            break;
                        }
                        Ok(Some(Ok(Message::Ping(data)))) => {
                            if let Err(e) = ws.send(Message::Pong(data)).await {
                                warn!("pong error for session {session_id}: {e}");
                                break;
                            }
                        }
                        Ok(Some(Err(e))) => {
                            warn!("WebSocket error for session {session_id}: {e}");
                            break;
                        }
                        Ok(None) => {
                            debug!("session {session_id} stream ended");
                            break;
                        }
                        Err(_) => {
                            warn!("session {session_id} message timeout");
                            break;
                        }
                        _ => {
                            // Ignore Binary, Pong, Frame, and other message types
                        }
                    }
                }
            }
        }

        // A query still running when the connection goes away is stopped:
        // nothing is listening for its answer, and it would keep using the
        // account and the filesystem otherwise.
        if let Some(handle) = running_queries.remove(&session_id) {
            handle.interrupt();
        }

        self.disconnect(&session_id);
        info!(
            "session {session_id} disconnected, {} active",
            self.session_count()
        );
    }

    /// Route a structured JSON message to the appropriate handler.
    ///
    /// A query does not return a response: it starts a task that streams the
    /// answer over `out`. Everything else answers immediately.
    fn handle_message(
        &self,
        session_id: &str,
        text: &str,
        out: &tokio::sync::mpsc::UnboundedSender<String>,
        running_queries: &mut HashMap<String, coderex_core::engine::InterruptHandle>,
    ) -> Option<WsResponse> {
        let req: WsRequest = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                return Some(WsResponse::error(
                    "unknown",
                    "parse_error",
                    &format!("invalid JSON request: {e}"),
                ));
            }
        };

        match req.kind {
            WsRequestKind::Query { content } => {
                debug!(
                    "query from session {session_id}: {}",
                    content.chars().take(80).collect::<String>()
                );

                let Some(factory) = self.engine_factory.clone() else {
                    return Some(WsResponse::error(
                        &req.id,
                        "no_engine",
                        "this server was started without a query engine, so there is nothing to \
                         run the query on",
                    ));
                };

                let (mut engine, mut events) = factory();
                let handle = engine.interrupt_handle();
                running_queries.insert(session_id.to_string(), handle.clone());

                let id = req.id.clone();
                let out = out.clone();
                let session = session_id.to_string();
                tokio::spawn(async move {
                    // The answer streams while the query runs, so the events
                    // are pumped on their own task.
                    let pump_id = id.clone();
                    let pump_out = out.clone();
                    let pump = tokio::spawn(async move {
                        while let Some(event) = events.recv().await {
                            if let EngineEvent::TextDelta(chunk) = event {
                                let frame = WsResponse {
                                    id: pump_id.clone(),
                                    kind: WsResponseKind::StreamChunk { chunk },
                                };
                                if let Ok(json) = serde_json::to_string(&frame) {
                                    if pump_out.send(json).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });

                    let result = engine
                        .submit_streaming(content, SubmitOptions::default())
                        .await;

                    // Dropping the engine closes the event channel, which ends
                    // the pump.
                    drop(engine);
                    let _ = pump.await;

                    let frame = match result {
                        Ok(reason) => WsResponse {
                            id,
                            kind: WsResponseKind::QueryDone {
                                reason: reason.to_string(),
                            },
                        },
                        Err(error) => WsResponse::error(&id, "query_failed", &error),
                    };
                    if let Ok(json) = serde_json::to_string(&frame) {
                        let _ = out.send(json);
                    }
                    // The handle stays until the session ends: `Interrupt`
                    // needs to find it while the query is in flight.
                    let _ = session;
                });

                None
            }
            WsRequestKind::Interrupt => {
                let interrupted = running_queries.get(session_id).is_some_and(|handle| {
                    handle.interrupt();
                    true
                });
                Some(WsResponse {
                    id: req.id,
                    kind: WsResponseKind::QueryDone {
                        reason: if interrupted {
                            "interrupted".to_string()
                        } else {
                            "nothing running".to_string()
                        },
                    },
                })
            }
            WsRequestKind::Ping => Some(WsResponse::pong(&req.id)),
        }
    }
}
/// Server for managing detached sessions over WebSocket.
pub struct DaemonServer {
    /// Server configuration.
    config: ServerConfig,
    /// Session bookkeeping, shared with the task serving each connection.
    shared: Arc<parking_lot::Mutex<Shared>>,
    /// Thread-safe session count for the health endpoint.
    session_count_atomic: Arc<AtomicUsize>,
    /// How a session's queries reach a model, when one was supplied.
    engine_factory: Option<EngineFactory>,
}

impl std::fmt::Debug for DaemonServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonServer")
            .field("sessions", &self.shared.lock().sessions.len())
            .field("has_engine", &self.engine_factory.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for DaemonServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonServer {
    /// Supply the engine sessions run their queries on.
    ///
    /// Without one, a query is refused with `no_engine` rather than answered:
    /// a server that cannot run a query has nothing true to say about one.
    pub fn set_engine_factory(&mut self, factory: EngineFactory) {
        self.engine_factory = Some(factory);
    }

    /// Create a new daemon server with default config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: ServerConfig::default(),
            shared: Arc::new(parking_lot::Mutex::new(Shared::default())),
            session_count_atomic: Arc::new(AtomicUsize::new(0)),
            engine_factory: None,
        }
    }

    /// Create a new daemon server with the given config.
    #[must_use]
    pub fn with_config(config: ServerConfig) -> Self {
        let mut server = Self {
            config: config.clone(),
            shared: Arc::new(parking_lot::Mutex::new(Shared::default())),
            session_count_atomic: Arc::new(AtomicUsize::new(0)),
            engine_factory: None,
        };

        // Initialize persistence if configured
        if let Some(ref path) = config.persist_path {
            match crate::persistence::SessionStore::open(path) {
                Ok(store) => {
                    // Restore previous sessions
                    let restored = store.load_all().unwrap_or_default();
                    let mut shared = server.shared.lock();
                    for ps in &restored {
                        let session = crate::persistence::restore_session(ps);
                        shared.sessions.insert(session.id.clone(), session);
                    }
                    if !restored.is_empty() {
                        info!("Restored {} sessions from database", restored.len());
                    }
                    shared.store = Some(store);
                }
                Err(e) => {
                    warn!("Failed to open session store at {:?}: {}", path, e);
                }
            }
        }

        server.sync_session_count();
        server
    }

    /// Authenticate a new session with the given token.
    pub fn authenticate(&mut self, token: &str) -> Result<String, String> {
        let id = self.shared.lock().authenticate(token, &self.config)?;
        self.sync_session_count();
        Ok(id)
    }

    /// Record activity for a session (resets idle timer).
    pub fn record_activity(&mut self, session_id: &str) -> bool {
        self.shared.lock().record_activity(session_id)
    }

    /// Remove idle sessions that have exceeded the timeout.
    pub fn cleanup_idle(&mut self) -> Vec<String> {
        let removed = self.shared.lock().cleanup_idle(self.config.idle_timeout);
        self.sync_session_count();
        removed
    }

    /// List all active sessions.
    #[must_use]
    /// The active sessions, as copies.
    ///
    /// Copies rather than references: the bookkeeping is behind a lock now, and
    /// a reference out of it would outlive the guard.
    pub fn list_sessions(&self) -> Vec<Session> {
        self.shared.lock().sessions.values().cloned().collect()
    }

    /// Get a session by ID.
    #[must_use]
    pub fn get_session(&self, session_id: &str) -> Option<Session> {
        self.shared.lock().sessions.get(session_id).cloned()
    }

    /// Disconnect a session.
    pub fn disconnect(&mut self, session_id: &str) -> bool {
        let removed = self.shared.lock().disconnect(session_id);
        if removed {
            self.sync_session_count();
        }
        removed
    }

    /// Number of active sessions.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.session_count_atomic.load(Ordering::Relaxed)
    }

    /// Update the atomic session counter (called after sessions change).
    fn sync_session_count(&self) {
        self.session_count_atomic
            .store(self.shared.lock().sessions.len(), Ordering::Relaxed);
    }

    /// Start the WebSocket server and accept connections.
    ///
    /// Binds a TCP listener on the given address, upgrades connections to
    /// WebSocket, authenticates clients, and routes messages.
    /// If `config.health_port` is set, spawns a lightweight HTTP server
    /// on that port serving a `/health` endpoint.
    pub async fn run(&mut self, addr: SocketAddr) -> Result<(), String> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("failed to bind TCP listener: {e}"))?;

        info!("WebSocket server listening on ws://{addr}");

        // Spawn health endpoint if configured
        if let Some(health_port) = self.config.health_port {
            let counter = Arc::clone(&self.session_count_atomic);
            tokio::spawn(async move {
                run_health_server(health_port, counter).await;
            });
        }

        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, peer)) => {
                            debug!("new TCP connection from {peer}");
                            // Served in its own task. Awaiting the connection
                            // here meant one client held the accept loop for as
                            // long as it stayed connected: no second client
                            // could connect, and the idle sweep above never
                            // ran.
                            let connection = Connection {
                                config: self.config.clone(),
                                shared: Arc::clone(&self.shared),
                                session_count_atomic: Arc::clone(&self.session_count_atomic),
                                engine_factory: self.engine_factory.clone(),
                            };
                            tokio::spawn(async move {
                                connection.serve(stream).await;
                            });
                        }
                        Err(e) => {
                            warn!("accept error: {e}");
                        }
                    }
                }
                _ = cleanup_interval.tick() => {
                    let removed = self.cleanup_idle();
                    if !removed.is_empty() {
                        info!("cleaned up {} idle sessions: {:?}", removed.len(), removed);
                    }
                }
            }
        }
    }
}

/// Lightweight HTTP server serving a single `/health` endpoint.
async fn run_health_server(port: u16, session_count: Arc<AtomicUsize>) {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!("failed to bind health endpoint on port {port}: {e}");
            return;
        }
    };
    info!("Health endpoint listening on http://0.0.0.0:{port}/health");

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let counter = Arc::clone(&session_count);
        tokio::spawn(async move {
            let _ = handle_health_request(stream, counter).await;
        });
    }
}

async fn handle_health_request(
    stream: TcpStream,
    session_count: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut reader, mut writer) = stream.into_split();

    // Read the HTTP request (just need the first line)
    let mut buf = [0u8; 1024];
    let n = reader.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");

    let status: u16;
    let body: serde_json::Value;

    if first_line.starts_with("GET /health") {
        status = 200;
        body = serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "sessions": session_count.load(Ordering::Relaxed),
        });
    } else {
        status = 404;
        body = serde_json::json!({ "error": "not found" });
    }

    let body_str = serde_json::to_string(&body).unwrap();
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_str}",
        if status == 200 { "OK" } else { "Not Found" },
        body_str.len(),
    );

    writer.write_all(response.as_bytes()).await?;
    writer.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_keys() -> DaemonServer {
        DaemonServer::with_config(ServerConfig {
            valid_keys: vec!["key1".to_string(), "key2".to_string()],
            allow_anonymous: false,
            idle_timeout: Duration::from_secs(1),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        })
    }

    #[test]
    fn authenticate_with_valid_key() {
        let mut server = server_with_keys();
        let id = server.authenticate("key1").unwrap();
        assert!(id.starts_with("sess_"));
        let session = server.get_session(&id).unwrap();
        assert!(session.authenticated);
        assert_eq!(session.auth_token, "key1");
    }

    #[test]
    fn authenticate_with_invalid_key() {
        let mut server = server_with_keys();
        let err = server.authenticate("badkey").unwrap_err();
        assert_eq!(err, "invalid API key");
    }

    #[test]
    fn unconfigured_server_refuses_every_token() {
        // No keys and no explicit opt-in: this used to accept anyone who could
        // reach the port.
        let mut server = DaemonServer::new();
        let err = server.authenticate("anything").unwrap_err();
        assert!(err.contains("no API keys"), "{err}");
        assert!(server.get_session("sess_1").is_none());
    }

    #[test]
    fn anonymous_access_is_an_explicit_opt_in() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_secs(1),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let id = server.authenticate("anything").unwrap();
        assert!(server.get_session(&id).is_some());
    }

    /// Ids must not be reused: a counter derived from `sessions.len()` repeats
    /// after a removal and overwrites a live session.
    #[test]
    fn session_ids_survive_a_removal() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_secs(1),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let first = server.authenticate("a").unwrap();
        let second = server.authenticate("b").unwrap();
        assert_ne!(first, second);

        assert!(server.disconnect(&first));

        let third = server.authenticate("c").unwrap();
        assert_ne!(third, second, "a removed session's id must not be reused");
        assert!(
            server.get_session(&second).is_some(),
            "the live session must not be overwritten"
        );
    }

    #[test]
    fn cleanup_removes_idle_sessions() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_millis(50),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let id = server.authenticate("tok").unwrap();
        // Wait for timeout
        std::thread::sleep(Duration::from_millis(60));
        let removed = server.cleanup_idle();
        assert_eq!(removed, vec![id]);
        assert_eq!(server.session_count(), 0);
    }

    #[test]
    fn record_activity_prevents_cleanup() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_millis(50),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let id = server.authenticate("tok").unwrap();
        // Activity right before timeout
        std::thread::sleep(Duration::from_millis(30));
        server.record_activity(&id);
        std::thread::sleep(Duration::from_millis(30));
        let removed = server.cleanup_idle();
        assert!(removed.is_empty());
        assert_eq!(server.session_count(), 1);
    }

    #[test]
    fn max_sessions_enforced() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_secs(3600),
            max_sessions: 2,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        server.authenticate("a").unwrap();
        server.authenticate("b").unwrap();
        let err = server.authenticate("c").unwrap_err();
        assert_eq!(err, "max sessions reached");
    }

    #[test]
    fn disconnect_removes_session() {
        let mut server = server_with_keys();
        let id = server.authenticate("key1").unwrap();
        assert!(server.disconnect(&id));
        assert!(!server.disconnect(&id)); // already gone
        assert_eq!(server.session_count(), 0);
    }

    #[test]
    fn list_sessions() {
        let mut server = server_with_keys();
        server.authenticate("key1").unwrap();
        server.authenticate("key2").unwrap();
        let sessions = server.list_sessions();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn token_bucket_allows_burst_then_limits() {
        // 5 tokens, refill 1/sec
        let mut bucket = TokenBucket::new(5, 1.0);
        // Should allow 5 consumes
        for _ in 0..5 {
            assert!(bucket.try_consume());
        }
        // 6th should fail (no time elapsed)
        assert!(!bucket.try_consume());
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(10, 100.0); // fast refill for test
        // Drain all tokens
        for _ in 0..10 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());
        // Wait a bit for refill
        std::thread::sleep(Duration::from_millis(50));
        // Should have refilled enough tokens
        assert!(bucket.try_consume());
    }

    #[test]
    fn protocol_serialize_query_request() {
        let req = WsRequest {
            id: "req_1".to_string(),
            kind: WsRequestKind::Query {
                content: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("req_1"));
        assert!(json.contains("hello"));
        assert!(json.contains("query"));
    }

    #[test]
    fn protocol_serialize_query_response() {
        let resp = WsResponse {
            id: "req_1".to_string(),
            kind: WsResponseKind::QueryOk {
                content: "hi back".to_string(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("query_ok"));
        assert!(json.contains("hi back"));
    }

    #[test]
    fn protocol_serialize_error_response() {
        let resp = WsResponse::error("req_1", "bad_request", "invalid JSON");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("error"));
        assert!(json.contains("bad_request"));
        assert!(json.contains("invalid JSON"));
    }

    #[test]
    fn protocol_serialize_event() {
        let evt = WsEvent::SessionReady {
            session_id: "sess_1".to_string(),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("session_ready"));
        assert!(json.contains("sess_1"));
    }

    #[test]
    fn protocol_deserialize_query_request() {
        let json = r#"{"id":"r1","type":"query","content":"test"}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "r1");
        match req.kind {
            WsRequestKind::Query { content } => assert_eq!(content, "test"),
            _ => panic!("expected Query variant"),
        }
    }

    #[test]
    fn protocol_deserialize_ping_request() {
        let json = r#"{"id":"r2","type":"ping"}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "r2");
        assert!(matches!(req.kind, WsRequestKind::Ping));
    }
}
