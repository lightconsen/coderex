//! Server / SDK integration tests — covering WebSocket sessions, structured I/O,
//! idle cleanup, and session listing.
//!
//! These tests correspond to the equivalence test plan Module 13:
//! 13.1 WebSocket session auth, 13.2 structured I/O, 13.3 idle cleanup, 13.4 session listing

use std::net::SocketAddr;
use std::time::Duration;

use coderex_server::{DaemonServer, ServerConfig, WsEvent};

/// A model that answers with fixed text, so the server's query path is
/// exercised without a provider.
struct ScriptedModel(&'static str);

#[async_trait::async_trait]
impl coderex_core::api_client::ApiClient for ScriptedModel {
    fn clone_box(&self) -> Box<dyn coderex_core::api_client::ApiClient> {
        Box::new(Self(self.0))
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &coderex_core::types::ModelId,
        _max_tokens: u32,
        _tools: &[coderex_core::tool_executor::ToolInfo],
    ) -> Result<coderex_core::api_client::ApiResponse, coderex_core::api_client::ApiError> {
        Ok(coderex_core::api_client::ApiResponse {
            content: self.0.to_string(),
            tool_calls: Vec::new(),
            thinking: None,
            usage: coderex_core::types::UsageSummary {
                input_tokens: 5,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            stop_reason: "end_turn".into(),
        })
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &coderex_core::types::ModelId,
        max_tokens: u32,
        tools: &[coderex_core::tool_executor::ToolInfo],
    ) -> std::pin::Pin<
        Box<dyn futures::Stream<Item = coderex_core::api_client::StreamEvent> + Send + 'static>,
    > {
        let response = self
            .create_message(system, messages, model, max_tokens, tools)
            .await
            .expect("the scripted model answers");
        Box::pin(futures::stream::iter(vec![
            coderex_core::api_client::StreamEvent::TextDelta(response.content),
            coderex_core::api_client::StreamEvent::StopReason(response.stop_reason),
            coderex_core::api_client::StreamEvent::StreamEnd,
        ]))
    }
}
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Start a DaemonServer on a free port and return the bound address.
async fn start_server(config: ServerConfig) -> SocketAddr {
    start_server_with(config, None).await
}

/// Start a server whose engine factories record the handle they handed out.
async fn start_server_recording_handles(
    config: ServerConfig,
    answer: &'static str,
    delay: Duration,
) -> (
    SocketAddr,
    std::sync::Arc<std::sync::Mutex<Vec<coderex_core::engine::InterruptHandle>>>,
) {
    let handles = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = std::sync::Arc::clone(&handles);

    let addr = start_server_with_factory(
        config,
        std::sync::Arc::new(move || {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut engine = coderex_core::engine::QueryEngine::new(
                Box::new(SlowScriptedModel { answer, delay }),
                coderex_core::types::ModelId::sonnet(),
                coderex_core::state::SessionState::new(std::path::PathBuf::from("/tmp")),
                coderex_core::permission_engine::PermissionEngine::new(
                    coderex_core::permissions::PermissionChecker::new(
                        coderex_core::permissions::PermissionMode::BypassPermissions,
                    ),
                ),
                tx,
            );
            engine.set_tool_executor(Box::new(coderex_core::tool_executor::NoOpToolExecutor));
            if let Ok(mut recorded) = recorder.lock() {
                recorded.push(engine.interrupt_handle());
            }
            (engine, rx)
        }),
    )
    .await;

    (addr, handles)
}

/// A model that answers slowly, so a query is still running when the socket
/// goes away.
struct SlowScriptedModel {
    answer: &'static str,
    delay: Duration,
}

#[async_trait::async_trait]
impl coderex_core::api_client::ApiClient for SlowScriptedModel {
    fn clone_box(&self) -> Box<dyn coderex_core::api_client::ApiClient> {
        Box::new(Self {
            answer: self.answer,
            delay: self.delay,
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &coderex_core::types::ModelId,
        _max_tokens: u32,
        _tools: &[coderex_core::tool_executor::ToolInfo],
    ) -> Result<coderex_core::api_client::ApiResponse, coderex_core::api_client::ApiError> {
        Ok(coderex_core::api_client::ApiResponse {
            content: self.answer.to_string(),
            tool_calls: Vec::new(),
            thinking: None,
            usage: coderex_core::types::UsageSummary {
                input_tokens: 5,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            stop_reason: "end_turn".into(),
        })
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &coderex_core::types::ModelId,
        max_tokens: u32,
        tools: &[coderex_core::tool_executor::ToolInfo],
    ) -> std::pin::Pin<
        Box<dyn futures::Stream<Item = coderex_core::api_client::StreamEvent> + Send + 'static>,
    > {
        let response = self
            .create_message(system, messages, model, max_tokens, tools)
            .await
            .expect("the slow model answers");
        let delay = self.delay;
        Box::pin(futures::stream::unfold(0, move |step| {
            let answer = response.content.clone();
            async move {
                match step {
                    0 => Some((
                        coderex_core::api_client::StreamEvent::TextDelta(answer),
                        step + 1,
                    )),
                    1 => {
                        // The answer takes a while, which is the window the
                        // disconnect has to land in.
                        tokio::time::sleep(delay).await;
                        Some((
                            coderex_core::api_client::StreamEvent::StopReason("end_turn".into()),
                            step + 1,
                        ))
                    }
                    2 => Some((coderex_core::api_client::StreamEvent::StreamEnd, step + 1)),
                    _ => None,
                }
            }
        }))
    }
}

/// Start a server with an engine factory, returning the address.
async fn start_server_with_factory(
    config: ServerConfig,
    factory: coderex_server::session::EngineFactory,
) -> SocketAddr {
    start_on_free_port(config, Some(factory)).await
}

/// Start a server, optionally with an engine whose model answers `answer`.
async fn start_server_with(config: ServerConfig, answer: Option<&'static str>) -> SocketAddr {
    let factory = answer.map(|answer| {
        std::sync::Arc::new(move || {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut engine = coderex_core::engine::QueryEngine::new(
                Box::new(ScriptedModel(answer)),
                coderex_core::types::ModelId::sonnet(),
                coderex_core::state::SessionState::new(std::path::PathBuf::from("/tmp")),
                coderex_core::permission_engine::PermissionEngine::new(
                    coderex_core::permissions::PermissionChecker::new(
                        coderex_core::permissions::PermissionMode::BypassPermissions,
                    ),
                ),
                tx,
            );
            engine.set_tool_executor(Box::new(coderex_core::tool_executor::NoOpToolExecutor));
            (engine, rx)
        }) as coderex_server::session::EngineFactory
    });

    start_on_free_port(config, factory).await
}

/// Start a server on the first free port in the test range.
async fn start_on_free_port(
    config: ServerConfig,
    factory: Option<coderex_server::session::EngineFactory>,
) -> SocketAddr {
    for port in 19000..20000 {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut server = DaemonServer::with_config(config.clone());
        if let Some(factory) = factory.clone() {
            server.set_engine_factory(factory);
        }
        let handle = tokio::spawn(async move {
            let _ = server.run(addr).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        if !handle.is_finished() {
            return addr;
        }
    }
    panic!("no free port found for test server");
}

/// Connect a WebSocket client to the server.
async fn ws_connect(addr: SocketAddr) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let url = format!("ws://{addr}");
    let (ws, _) = tokio_tungstenite::client_async(url, stream).await.unwrap();
    ws
}

/// Authenticate via WebSocket and return the session ID.
async fn ws_auth(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    token: &str,
) -> String {
    let auth = serde_json::json!({"token": token});
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();

    // Read session_id response
    let text = ws_recv_text(ws).await;
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let session_id = parsed
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Read and verify SessionReady event
    let ready_text = ws_recv_text(ws).await;
    let ready: serde_json::Value = serde_json::from_str(&ready_text).unwrap();
    assert_eq!(ready.get("event").unwrap(), "session_ready");
    assert_eq!(ready.get("session_id").unwrap(), &session_id);

    session_id
}

/// Send a JSON text message over WebSocket.
async fn ws_send_json(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    value: &serde_json::Value,
) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

/// Receive the next text message from WebSocket (with timeout).
async fn ws_recv_text(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> String {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("ws recv timeout")
        .expect("ws stream ended")
        .expect("ws error");
    match msg {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text message, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  13.1 WebSocket Session Auth
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn websocket_auth_valid_token_creates_session() {
    let config = ServerConfig {
        valid_keys: vec!["secret-key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;

    let session_id = ws_auth(&mut ws, "secret-key").await;
    assert!(session_id.starts_with("sess_"));
}

#[tokio::test]
async fn websocket_auth_invalid_token_rejected() {
    let config = ServerConfig {
        valid_keys: vec!["secret-key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;

    let auth = serde_json::json!({"token": "bad-key"});
    ws_send_json(&mut ws, &auth).await;

    let text = ws_recv_text(&mut ws).await;
    assert!(
        text.contains("error") && text.contains("invalid API key"),
        "expected auth error, got: {text}"
    );
}

#[tokio::test]
async fn websocket_auth_max_sessions_enforced() {
    // NOTE: The current server handles connections sequentially (one at a time),
    // so we test max_sessions through the direct API rather than concurrent WS.
    let mut server = DaemonServer::with_config(ServerConfig {
        valid_keys: vec!["key1".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 2,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    });

    server.authenticate("key1").unwrap();
    server.authenticate("key1").unwrap();

    let err = server.authenticate("key1").unwrap_err();
    assert_eq!(err, "max sessions reached");
}

#[tokio::test]
async fn websocket_auth_raw_token_fallback() {
    let config = ServerConfig {
        valid_keys: vec!["my-token".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;

    // Send raw token string (not JSON)
    ws.send(Message::Text("my-token".into())).await.unwrap();

    let text = ws_recv_text(&mut ws).await;
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        parsed.get("session_id").is_some(),
        "expected session_id, got: {text}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  13.2 Structured I/O
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn structured_io_query_request_response() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server_with(config, Some("the scripted answer")).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;

    let req = serde_json::json!({
        "id": "req_1",
        "type": "query",
        "content": "hello world"
    });
    ws_send_json(&mut ws, &req).await;

    // The answer arrives as streamed chunks — the query ran — and then the
    // query ends. A server with no engine refuses instead (see
    // `a_query_without_an_engine_is_refused_rather_than_echoed`).
    let mut answer = String::new();
    loop {
        let text = ws_recv_text(&mut ws).await;
        let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(frame.get("id").unwrap(), "req_1");
        match frame.get("type").unwrap().as_str().unwrap() {
            "stream_chunk" => {
                answer.push_str(frame.get("chunk").unwrap().as_str().unwrap());
            }
            "query_done" => {
                assert_eq!(frame.get("reason").unwrap(), "completed");
                break;
            }
            other => panic!("unexpected frame: {other}"),
        }
    }

    assert_eq!(answer, "the scripted answer", "the streamed answer");
}

/// Ported from qwen-code's channel-plugin contract: messages on one session
/// share that session's history, while a second session starts clean. The
/// scripted model reports how many provider messages it received, so the
/// assertion is about what the server actually sent, not about echoed text.
#[tokio::test]
async fn a_session_reuses_its_history_and_a_new_session_starts_clean() {
    struct HistoryEcho;

    #[async_trait::async_trait]
    impl coderex_core::api_client::ApiClient for HistoryEcho {
        fn clone_box(&self) -> Box<dyn coderex_core::api_client::ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            messages: &[serde_json::Value],
            _model: &coderex_core::types::ModelId,
            _max_tokens: u32,
            _tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> Result<coderex_core::api_client::ApiResponse, coderex_core::api_client::ApiError>
        {
            Ok(coderex_core::api_client::ApiResponse {
                content: format!("history={}", messages.len()),
                tool_calls: Vec::new(),
                thinking: None,
                usage: coderex_core::types::UsageSummary::default(),
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            system: &str,
            messages: &[serde_json::Value],
            model: &coderex_core::types::ModelId,
            max_tokens: u32,
            tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = coderex_core::api_client::StreamEvent> + Send + 'static>,
        > {
            let response = self
                .create_message(system, messages, model, max_tokens, tools)
                .await
                .expect("history echo answers");
            Box::pin(futures::stream::iter(vec![
                coderex_core::api_client::StreamEvent::TextDelta(response.content),
                coderex_core::api_client::StreamEvent::StopReason(response.stop_reason),
                coderex_core::api_client::StreamEvent::StreamEnd,
            ]))
        }
    }

    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let factory: coderex_server::session::EngineFactory = std::sync::Arc::new(|| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut engine = coderex_core::engine::QueryEngine::new(
            Box::new(HistoryEcho),
            coderex_core::types::ModelId::sonnet(),
            coderex_core::state::SessionState::new(std::path::PathBuf::from("/tmp")),
            coderex_core::permission_engine::PermissionEngine::new(
                coderex_core::permissions::PermissionChecker::new(
                    coderex_core::permissions::PermissionMode::BypassPermissions,
                ),
            ),
            tx,
        );
        engine.set_tool_executor(Box::new(coderex_core::tool_executor::NoOpToolExecutor));
        (engine, rx)
    });
    let addr = start_server_with_factory(config, factory).await;

    async fn ask(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        id: &str,
        content: &str,
    ) -> String {
        ws_send_json(
            ws,
            &serde_json::json!({"id": id, "type": "query", "content": content}),
        )
        .await;
        let mut answer = String::new();
        loop {
            let text = ws_recv_text(ws).await;
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            match frame.get("type").and_then(|value| value.as_str()) {
                Some("stream_chunk") => {
                    answer.push_str(frame.get("chunk").and_then(|v| v.as_str()).unwrap_or(""));
                }
                Some("query_done") => break,
                other => panic!("unexpected frame: {other:?} ({text})"),
            }
        }
        answer
    }

    let mut first = ws_connect(addr).await;
    let _sid = ws_auth(&mut first, "key").await;
    assert_eq!(
        ask(&mut first, "q1", "remember pineapple").await,
        "history=1"
    );
    assert_eq!(
        ask(&mut first, "q2", "what fruit?").await,
        "history=3",
        "the second query must carry the first exchange"
    );

    // The server accepts one connection at a time; release the first before
    // opening the second session.
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut second = ws_connect(addr).await;
    let _sid = ws_auth(&mut second, "key").await;
    assert_eq!(
        ask(&mut second, "q3", "what fruit?").await,
        "history=1",
        "a new session must not inherit another session's history"
    );
}

/// A query still running when the connection goes away is stopped: nothing is
/// listening for its answer, and it would keep using the account otherwise.
#[tokio::test]
async fn a_disconnect_stops_the_query_that_was_running() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let (addr, handles) =
        start_server_recording_handles(config, "a slow answer", Duration::from_secs(30)).await;

    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;
    ws_send_json(
        &mut ws,
        &serde_json::json!({"id": "q1", "type": "query", "content": "say something"}),
    )
    .await;

    // Wait for the query to start, then leave.
    let mut waited = 0;
    while handles.lock().map_or(true, |held| held.is_empty()) && waited < 100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }
    let handle = handles
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("the engine factory ran");
    assert!(
        !handle.is_interrupted(),
        "interrupted before the disconnect"
    );

    ws.close(None).await.unwrap();

    let mut waited = 0;
    while !handle.is_interrupted() && waited < 100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        waited += 1;
    }
    assert!(
        handle.is_interrupted(),
        "the query kept running after the client left"
    );
}

/// The session store must not hold the key that opened the session: the file
/// gets backed up and synced, and a working credential for this daemon in it
/// is a leak that outlives the process.
#[tokio::test]
async fn the_session_store_never_holds_the_key() {
    let db = std::env::temp_dir().join(format!("coderex_store_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);

    let config = ServerConfig {
        valid_keys: vec!["the-actual-key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: Some(db.clone()),
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "the-actual-key").await;
    ws.close(None).await.unwrap();

    // The write happens on the connection's thread; give it a moment.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let bytes = std::fs::read(&db).expect("the store was written");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("the-actual-key"),
        "the key that opened the session is in the store file"
    );
    assert!(
        text.contains("sess_"),
        "the session was not persisted at all"
    );

    let _ = std::fs::remove_file(&db);
}

/// One connection must not hold the accept loop. It used to: the loop awaited
/// the whole connection, so a client that connected and stayed quiet kept
/// every other client out — and kept the idle sweep from running.
#[tokio::test]
async fn a_quiet_connection_does_not_block_the_next_one() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;

    // A connection that authenticates and then just sits there.
    let mut quiet = ws_connect(addr).await;
    let _sid = ws_auth(&mut quiet, "key").await;

    // A second client must still be served.
    let mut second = tokio::time::timeout(Duration::from_secs(5), ws_connect(addr))
        .await
        .expect("a second client could not connect while the first was idle");
    let answer = tokio::time::timeout(Duration::from_secs(5), ws_auth(&mut second, "key"))
        .await
        .expect("a second client could not authenticate");
    assert!(answer.starts_with("sess_"));

    quiet.close(None).await.ok();
    second.close(None).await.ok();
}

#[tokio::test]
async fn a_query_without_an_engine_is_refused_rather_than_echoed() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;

    ws_send_json(
        &mut ws,
        &serde_json::json!({"id": "q1", "type": "query", "content": "hello"}),
    )
    .await;

    let text = ws_recv_text(&mut ws).await;
    let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(frame.get("type").unwrap(), "error");
    assert_eq!(frame.get("code").unwrap(), "no_engine");
    assert!(
        frame
            .get("message")
            .and_then(|m| m.as_str())
            .is_some_and(|m| m.contains("query engine")),
        "{frame}"
    );
}

#[tokio::test]
async fn structured_io_ping_pong() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;

    let req = serde_json::json!({"id": "ping_1", "type": "ping"});
    ws_send_json(&mut ws, &req).await;

    let resp_text = ws_recv_text(&mut ws).await;
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(resp.get("id").unwrap(), "ping_1");
    assert_eq!(resp.get("type").unwrap(), "pong");
}

#[tokio::test]
async fn structured_io_interrupt_returns_done() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;

    let req = serde_json::json!({"id": "int_1", "type": "interrupt"});
    ws_send_json(&mut ws, &req).await;

    let resp_text = ws_recv_text(&mut ws).await;
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(resp.get("id").unwrap(), "int_1");
    assert_eq!(resp.get("type").unwrap(), "query_done");
}

#[tokio::test]
async fn structured_io_invalid_json_error_response() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;

    ws.send(Message::Text("this is not json".into()))
        .await
        .unwrap();

    let resp_text = ws_recv_text(&mut ws).await;
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(resp.get("type").unwrap(), "error");
    assert!(
        resp.get("message")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("invalid JSON")
    );
}

#[tokio::test]
async fn structured_io_rate_limit_event() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 3, // 3 tokens only
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;

    // Send 3 ping requests (should all succeed)
    for i in 0..3 {
        let req = serde_json::json!({"id": format!("p{i}"), "type": "ping"});
        ws_send_json(&mut ws, &req).await;
        let resp = ws_recv_text(&mut ws).await;
        let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            parsed.get("type").unwrap(),
            "pong",
            "request {i} should succeed"
        );
    }

    // 4th request should trigger rate limit
    let req = serde_json::json!({"id": "p3", "type": "ping"});
    ws_send_json(&mut ws, &req).await;
    let resp = ws_recv_text(&mut ws).await;
    assert!(
        resp.contains("rate_limited"),
        "expected rate limit event, got: {resp}"
    );
}

#[tokio::test]
async fn structured_io_event_serialization_format() {
    // Verify WsEvent serializes with correct tag
    let evt = WsEvent::SessionReady {
        session_id: "s1".into(),
    };
    let json = serde_json::to_string(&evt).unwrap();
    assert!(json.contains("\"event\":\"session_ready\""));
    assert!(json.contains("\"session_id\":\"s1\""));

    let evt = WsEvent::IdleWarning {
        message: "timeout soon".into(),
    };
    let json = serde_json::to_string(&evt).unwrap();
    assert!(json.contains("\"event\":\"idle_warning\""));

    let evt = WsEvent::SessionClosed {
        reason: "done".into(),
    };
    let json = serde_json::to_string(&evt).unwrap();
    assert!(json.contains("\"event\":\"session_closed\""));
}

// ═══════════════════════════════════════════════════════════════════════════
//  13.3 Idle Cleanup
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn idle_cleanup_removes_inactive_session() {
    let mut server = DaemonServer::with_config(ServerConfig {
        valid_keys: vec![],
        idle_timeout: Duration::from_millis(50),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: true,
    });

    let id = server.authenticate("any-token").unwrap();
    assert_eq!(server.session_count(), 1);

    // Wait for idle timeout
    tokio::time::sleep(Duration::from_millis(70)).await;

    let removed = server.cleanup_idle();
    assert_eq!(removed, vec![id]);
    assert_eq!(server.session_count(), 0);
}

#[tokio::test]
async fn idle_cleanup_activity_resets_timer() {
    let mut server = DaemonServer::with_config(ServerConfig {
        valid_keys: vec![],
        idle_timeout: Duration::from_millis(100),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: true,
    });

    let id = server.authenticate("tok").unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.record_activity(&id);
    tokio::time::sleep(Duration::from_millis(60)).await;

    let removed = server.cleanup_idle();
    assert!(
        removed.is_empty(),
        "session should not be cleaned up after activity"
    );
    assert_eq!(server.session_count(), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  13.4 Session Listing
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_listing_active_sessions() {
    let mut server = DaemonServer::with_config(ServerConfig {
        valid_keys: vec!["k1".into(), "k2".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    });

    server.authenticate("k1").unwrap();
    server.authenticate("k2").unwrap();

    let sessions = server.list_sessions();
    assert_eq!(sessions.len(), 2);
}

#[tokio::test]
async fn session_listing_after_disconnect() {
    let mut server = DaemonServer::with_config(ServerConfig {
        valid_keys: vec!["k1".into(), "k2".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    });

    let id1 = server.authenticate("k1").unwrap();
    let _id2 = server.authenticate("k2").unwrap();

    server.disconnect(&id1);

    let sessions = server.list_sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].auth_token, "k2");
}

#[tokio::test]
async fn session_listing_count_atomic() {
    let mut server = DaemonServer::with_config(ServerConfig {
        valid_keys: vec![],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: true,
    });

    assert_eq!(server.session_count(), 0);
    server.authenticate("a").unwrap();
    assert_eq!(server.session_count(), 1);
    server.authenticate("b").unwrap();
    assert_eq!(server.session_count(), 2);
}

// ═══════════════════════════════════════════════════════════════════════════
//  13.5 Command resolution — the whitelists on the live path
// ═══════════════════════════════════════════════════════════════════════════

/// Authenticate with an explicit session role.
async fn ws_auth_as(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    token: &str,
    role: &str,
) -> String {
    ws_send_json(ws, &serde_json::json!({"token": token, "role": role})).await;
    let text = ws_recv_text(ws).await;
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let session_id = parsed
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let ready = ws_recv_text(ws).await;
    let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
    assert_eq!(ready["event"], "session_ready");
    session_id
}

async fn resolve_command(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    id: &str,
    line: &str,
) -> serde_json::Value {
    ws_send_json(
        ws,
        &serde_json::json!({"id": id, "type": "command", "line": line}),
    )
    .await;
    serde_json::from_str(&ws_recv_text(ws).await).unwrap()
}

fn command_test_config() -> ServerConfig {
    ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    }
}

/// A remote session cannot resolve a command off the whitelist, and the
/// refusal comes back over the same protocol the request arrived on.
#[tokio::test]
async fn a_remote_session_resolves_only_whitelisted_commands() {
    let addr = start_server(command_test_config()).await;
    let mut ws = ws_connect(addr).await;
    ws_auth(&mut ws, "key").await;

    let resolved = resolve_command(&mut ws, "c1", "ls -la").await;
    assert_eq!(resolved["type"], "command_resolved", "{resolved}");
    assert_eq!(resolved["command"], "ls -la");

    let refused = resolve_command(&mut ws, "c2", "rm -rf /").await;
    assert_eq!(refused["type"], "error", "{refused}");
    assert_eq!(refused["code"], "command_not_allowed");
}

#[tokio::test]
async fn a_remote_session_cannot_resolve_a_command_that_writes() {
    let addr = start_server(command_test_config()).await;
    let mut ws = ws_connect(addr).await;
    ws_auth(&mut ws, "key").await;

    let refused = resolve_command(&mut ws, "c1", "mkdir scratch").await;
    assert_eq!(refused["code"], "command_not_allowed", "{refused}");
}

#[tokio::test]
async fn a_bridge_session_gets_the_wider_whitelist() {
    let addr = start_server(command_test_config()).await;
    let mut ws = ws_connect(addr).await;
    ws_auth_as(&mut ws, "key", "bridge").await;

    // Writes are allowed inside a bridge session's working directory.
    let resolved = resolve_command(&mut ws, "c1", "mkdir scratch").await;
    assert_eq!(resolved["type"], "command_resolved", "{resolved}");

    // Destruction is not, at either level.
    let refused = resolve_command(&mut ws, "c2", "rm -rf /").await;
    assert_eq!(refused["code"], "command_not_allowed", "{refused}");
}

/// Slash commands are gated by the registry's availability matrix, not by the
/// shell-command whitelist.
#[tokio::test]
async fn slash_commands_are_gated_by_availability() {
    let addr = start_server(command_test_config()).await;

    // A remote session sees the commands flagged remote-safe.
    let mut remote = ws_connect(addr).await;
    ws_auth_as(&mut remote, "key", "remote").await;

    let resolved = resolve_command(&mut remote, "c1", "/clear").await;
    assert_eq!(resolved["type"], "command_resolved", "{resolved}");
    assert_eq!(resolved["command"], "clear");

    // `/bridge-kick` is remote-safe, so a remote session still gets it.
    let kick = resolve_command(&mut remote, "c2", "/bridge-kick").await;
    assert_eq!(kick["type"], "command_resolved", "{kick}");

    // `/heapdump` is not non-interactive-safe, so nobody gets it.
    let refused = resolve_command(&mut remote, "c3", "/heapdump").await;
    assert_eq!(refused["code"], "command_not_allowed", "{refused}");

    // A bridge session does not: the same command is not bridge-safe. This is
    // the role filter, not the non-interactive one — `/bridge-kick` passes the
    // latter for every role.
    let mut bridge = ws_connect(addr).await;
    ws_auth_as(&mut bridge, "key", "bridge").await;
    let refused = resolve_command(&mut bridge, "c4", "/bridge-kick").await;
    assert_eq!(refused["code"], "command_not_allowed", "{refused}");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Full WebSocket lifecycle
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn full_websocket_lifecycle_auth_query_disconnect() {
    let config = ServerConfig {
        valid_keys: vec!["key".into()],
        idle_timeout: Duration::from_secs(3600),
        max_sessions: 10,
        rate_limit_per_minute: 100,
        health_port: None,
        persist_path: None,
        allow_anonymous: false,
    };
    let addr = start_server_with(config, Some("lifecycle answer")).await;
    let mut ws = ws_connect(addr).await;

    // Auth
    let session_id = ws_auth(&mut ws, "key").await;
    assert!(session_id.starts_with("sess_"));

    // Send query: the answer streams, then the query ends.
    let req = serde_json::json!({
        "id": "q1",
        "type": "query",
        "content": "test query"
    });
    ws_send_json(&mut ws, &req).await;

    let mut answer = String::new();
    loop {
        let text = ws_recv_text(&mut ws).await;
        let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
        match frame.get("type").unwrap().as_str().unwrap() {
            "stream_chunk" => answer.push_str(frame.get("chunk").unwrap().as_str().unwrap()),
            "query_done" => break,
            other => panic!("unexpected frame: {other}"),
        }
    }
    assert_eq!(answer, "lifecycle answer");

    // Send ping
    let req = serde_json::json!({"id": "p1", "type": "ping"});
    ws_send_json(&mut ws, &req).await;

    let resp_text = ws_recv_text(&mut ws).await;
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(resp.get("type").unwrap(), "pong");

    // Graceful close
    ws.close(None).await.unwrap();
}
