//! `coderex serve` through the real binary: start the daemon subcommand, accept
//! a WebSocket client, authenticate it, and answer a query with the configured
//! provider.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn start_static_provider() -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args([
            "--port",
            "0",
            "--responses",
            responses.to_str().expect("responses path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn static provider");

    let stderr = child.stderr.take().expect("server stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut announced = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !announced && let Some(address) = line.split("listening on ").nth(1) {
                let _ = tx.send(address.trim().to_string());
                announced = true;
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("static provider did not announce its address");
    (child, format!("http://{address}"))
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

async fn connect(port: u16) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            let url = format!("ws://127.0.0.1:{port}");
            let (ws, _) = tokio_tungstenite::client_async(url, stream)
                .await
                .expect("websocket handshake");
            return ws;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the serve subcommand never listened on port {port}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn recv_text(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> String {
    let message = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("ws recv timeout")
        .expect("ws closed")
        .expect("ws error");
    match message {
        Message::Text(text) => text.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn serve_authenticates_and_answers_a_query() {
    let (mut provider, base_url) = start_static_provider();
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let workspace = tempfile::tempdir().expect("workspace");
    let port = free_port();

    let mut server = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["serve", "--port", &port.to_string()])
        .current_dir(workspace.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
        .env("CODEREX_API_KEYS", "test-key")
        .env("CODEREX_SERVER_NO_PERSIST", "1")
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex serve");

    let mut ws = connect(port).await;

    // Authenticate.
    ws.send(Message::Text(
        serde_json::json!({"token": "test-key"}).to_string().into(),
    ))
    .await
    .expect("auth send");
    let auth: serde_json::Value =
        serde_json::from_str(&recv_text(&mut ws).await).expect("auth json");
    let session_id = auth["session_id"].as_str().expect("session id").to_string();
    assert_ne!(session_id, "", "the server returned an empty session id");
    let ready: serde_json::Value =
        serde_json::from_str(&recv_text(&mut ws).await).expect("ready json");
    assert_eq!(ready["event"], "session_ready");

    // Ask a question and collect the streamed answer.
    ws.send(Message::Text(
        serde_json::json!({"id": "req_1", "type": "query", "content": "hello"})
            .to_string()
            .into(),
    ))
    .await
    .expect("query send");

    let mut answer = String::new();
    loop {
        let text = recv_text(&mut ws).await;
        let frame: serde_json::Value = serde_json::from_str(&text).expect("frame");
        assert_eq!(frame["id"], "req_1");
        match frame["type"].as_str().expect("type") {
            "stream_chunk" => answer.push_str(frame["chunk"].as_str().unwrap_or_default()),
            "query_done" => {
                assert_eq!(frame["reason"], "completed");
                break;
            }
            other => panic!("unexpected frame {other}: {frame}"),
        }
    }

    assert!(
        answer.starts_with("Hello!"),
        "the served query did not answer: {answer}"
    );

    drop(ws);
    let _ = server.kill();
    let _ = server.wait();
    let _ = provider.kill();
    let _ = provider.wait();
}
