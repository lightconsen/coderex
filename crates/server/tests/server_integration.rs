//! Server / SDK integration tests — covering WebSocket sessions, structured I/O,
//! idle cleanup, and session listing.
//!
//! These tests correspond to the equivalence test plan Module 13:
//! 13.1 WebSocket session auth, 13.2 structured I/O, 13.3 idle cleanup, 13.4 session listing

use std::net::SocketAddr;
use std::time::Duration;

use coderex_server::{DaemonServer, ServerConfig, WsEvent};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Start a DaemonServer on a free port and return the bound address.
async fn start_server(config: ServerConfig) -> SocketAddr {
    for port in 19000..20000 {
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut server = DaemonServer::with_config(config.clone());
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
async fn ws_connect(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
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
    ws.send(Message::Text(auth.to_string().into())).await.unwrap();

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
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;

    // Send raw token string (not JSON)
    ws.send(Message::Text("my-token".into())).await.unwrap();

    let text = ws_recv_text(&mut ws).await;
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(parsed.get("session_id").is_some(), "expected session_id, got: {text}");
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
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;
    let _sid = ws_auth(&mut ws, "key").await;

    let req = serde_json::json!({
        "id": "req_1",
        "type": "query",
        "content": "hello world"
    });
    ws_send_json(&mut ws, &req).await;

    let resp_text = ws_recv_text(&mut ws).await;
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(resp.get("id").unwrap(), "req_1");
    assert_eq!(resp.get("type").unwrap(), "query_ok");
    assert!(resp.get("content").is_some());
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
    assert!(resp
        .get("message")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("invalid JSON"));
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
    });

    let id = server.authenticate("tok").unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.record_activity(&id);
    tokio::time::sleep(Duration::from_millis(60)).await;

    let removed = server.cleanup_idle();
    assert!(removed.is_empty(), "session should not be cleaned up after activity");
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
    });

    assert_eq!(server.session_count(), 0);
    server.authenticate("a").unwrap();
    assert_eq!(server.session_count(), 1);
    server.authenticate("b").unwrap();
    assert_eq!(server.session_count(), 2);
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
    };
    let addr = start_server(config).await;
    let mut ws = ws_connect(addr).await;

    // Auth
    let session_id = ws_auth(&mut ws, "key").await;
    assert!(session_id.starts_with("sess_"));

    // Send query
    let req = serde_json::json!({
        "id": "q1",
        "type": "query",
        "content": "test query"
    });
    ws_send_json(&mut ws, &req).await;

    let resp_text = ws_recv_text(&mut ws).await;
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(resp.get("type").unwrap(), "query_ok");

    // Send ping
    let req = serde_json::json!({"id": "p1", "type": "ping"});
    ws_send_json(&mut ws, &req).await;

    let resp_text = ws_recv_text(&mut ws).await;
    let resp: serde_json::Value = serde_json::from_str(&resp_text).unwrap();
    assert_eq!(resp.get("type").unwrap(), "pong");

    // Graceful close
    ws.close(None).await.unwrap();
}
