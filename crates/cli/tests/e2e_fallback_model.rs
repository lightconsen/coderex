//! `--fallback-model` through the real binary: when the primary model is
//! reported unavailable, the same request is retried once on the fallback and
//! the rest of the session stays there.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_request(socket: &mut TcpStream) -> serde_json::Value {
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
    serde_json::from_slice(&buffer[header_end..header_end + length]).expect("request body")
}

async fn write_response(socket: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write response");
    socket.shutdown().await.expect("shutdown");
}

fn sse_answer(text: &str) -> String {
    let event = |name: &str, value: serde_json::Value| {
        format!(
            "event: {name}\ndata: {}\n\n",
            serde_json::to_string(&value).expect("event json")
        )
    };
    [
        event(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        ),
        event(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text},
            }),
        ),
        event(
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        event(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }),
        ),
        event("message_stop", serde_json::json!({"type": "message_stop"})),
    ]
    .concat()
}

#[tokio::test]
async fn the_primary_404_retries_once_on_the_fallback_model() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let models = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = std::sync::Arc::clone(&models);

    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(10), listener.accept()).await
            else {
                break;
            };
            let body = read_request(&mut socket).await;
            let model = body["model"].as_str().unwrap_or_default().to_string();
            seen.lock().unwrap().push(model.clone());
            if model == "primary-model" {
                write_response(
                    &mut socket,
                    "404 Not Found",
                    "application/json",
                    r#"{"type":"error","error":{"type":"not_found_error","message":"model not found"}}"#,
                )
                .await;
            } else {
                write_response(
                    &mut socket,
                    "200 OK",
                    "text/event-stream",
                    &sse_answer("fallback answer"),
                )
                .await;
            }
        }
    });

    let dir = tempfile::tempdir().expect("workspace");
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let base_for_child = base_url.clone();
    let cwd = dir.path().to_path_buf();
    let output =
        tokio::task::spawn_blocking(move || run(&cwd, &base_for_child, config_home.path()))
            .await
            .expect("join");
    server.abort();
    let _ = server.await;
    let models = models.lock().unwrap().clone();

    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(models.len() >= 2, "no retry happened: {models:?}");
    assert_eq!(models[0], "primary-model");
    assert_eq!(models[1], "fallback-model", "{models:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("fallback answer"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn run(cwd: &Path, base_url: &str, config_home: &Path) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args([
            "-p",
            "--model",
            "primary-model",
            "--fallback-model",
            "fallback-model",
        ])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "primary-model")
        .env("CLAUDE_CONFIG_DIR", config_home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}
