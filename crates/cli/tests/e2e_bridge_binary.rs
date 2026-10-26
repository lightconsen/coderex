//! `coderex bridge` through the real binary: register with a fake control
//! plane, poll an assignment, spawn the session process, and relay its output
//! over the WebSocket transport.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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
    let (tx, rx) = std::sync::mpsc::channel();
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

async fn read_http(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let read = socket.read(&mut chunk).await.expect("read");
        assert!(read > 0, "closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let mut chunk = vec![0u8; length.max(1)];
        let read = socket.read(&mut chunk).await.expect("read body");
        assert!(read > 0, "closed before body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    (headers, buffer[header_end..header_end + length].to_vec())
}

async fn write_http(socket: &mut TcpStream, body: &str) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
    socket.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn bridge_runs_an_assignment_and_relays_its_output() {
    let repo = tempfile::tempdir().expect("repo");
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let (mut provider, provider_url) = start_static_provider();

    // ── fake control plane ────────────────────────────────────────────────
    let http = TcpListener::bind("127.0.0.1:0").await.expect("http bind");
    let http_port = http.local_addr().expect("addr").port();
    let assignment = serde_json::json!({
        "assignment_id": "a1",
        "session_id": "s1",
        "working_directory": repo.path().display().to_string(),
        "prompt": "hello",
        "spawn_mode": "single_session"
    });
    let served = Arc::new(Mutex::new(false));
    let served_task = Arc::clone(&served);
    let http_task = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(20), http.accept()).await
            else {
                break;
            };
            let (headers, _) = read_http(&mut socket).await;
            let request_line = headers.lines().next().unwrap_or_default().to_string();
            let body = if request_line.contains("/api/environments/register") {
                r#"{"worker_epoch":1}"#.to_string()
            } else if request_line.contains("/claim") || request_line.contains("/capacity") {
                "{}".to_string()
            } else if request_line.contains("/assignments") {
                let mut served = served_task.lock().unwrap();
                let body = if *served {
                    r#"{"assignments":[],"count":0}"#.to_string()
                } else {
                    *served = true;
                    serde_json::json!({"assignments": [assignment], "count": 1}).to_string()
                };
                drop(served);
                body
            } else {
                "{}".to_string()
            };
            write_http(&mut socket, &body).await;
        }
    });

    // ── fake gateway (WebSocket) ──────────────────────────────────────────
    let ws = TcpListener::bind("127.0.0.1:0").await.expect("ws bind");
    let ws_port = ws.local_addr().expect("addr").port();
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let ws_task = tokio::spawn(async move {
        let Ok(Ok((socket, _))) = tokio::time::timeout(Duration::from_secs(20), ws.accept()).await
        else {
            return;
        };
        let Ok(mut websocket) = tokio_tungstenite::accept_async(socket).await else {
            return;
        };
        while let Some(Ok(message)) = websocket.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            match frame["type"].as_str().unwrap_or_default() {
                "heartbeat" => {
                    let ack = serde_json::json!({
                        "type": "heartbeat_ack",
                        "correlation_id": frame["correlation_id"],
                        "payload": "{}",
                        "metadata": {}
                    });
                    let _ = websocket.send(Message::Text(ack.to_string().into())).await;
                }
                "session_output" => {
                    if let Some(payload) = frame["payload"].as_str()
                        && let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload)
                        && let Some(line) = payload["line"].as_str()
                    {
                        let _ = line_tx.send(line.to_string());
                    }
                }
                _ => {}
            }
        }
    });

    // ── run the bridge ───────────────────────────────────────────────────
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .unwrap();
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args([
            "bridge",
            "--control-plane-url",
            &format!("http://127.0.0.1:{http_port}"),
            "--env-id",
            "test-env",
            "--transport-url",
            &format!("ws://127.0.0.1:{ws_port}"),
            "--repo-root",
            repo.path().to_str().unwrap(),
            "--max-concurrent",
            "1",
            "--poll-interval-secs",
            "1",
            "--heartbeat-interval-secs",
            "1",
            "--heartbeat-timeout-secs",
            "3",
        ])
        .current_dir(repo.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &provider_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex bridge");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut relayed = Vec::new();
    let mut saw_answer = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(5), line_rx.recv()).await {
            let answer = line.contains("Hello!");
            relayed.push(line);
            if answer {
                saw_answer = true;
                break;
            }
        }
    }
    let _ = bridge.kill();
    let _ = bridge.wait();
    let _ = provider.kill();
    let _ = provider.wait();
    http_task.abort();
    ws_task.abort();
    let _ = http_task.await;
    let _ = ws_task.await;

    assert!(
        saw_answer,
        "the session's answer never reached the gateway; relayed lines: {relayed:?}"
    );
}
