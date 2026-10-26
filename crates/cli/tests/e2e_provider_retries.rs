//! Provider failure modes through the real binary: a rate limit and a server
//! error are retried, and a slow stream still completes.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_body(socket: &mut TcpStream) -> serde_json::Value {
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

fn text_events(text: &str) -> Vec<String> {
    let event = |name: &str, value: serde_json::Value| {
        format!(
            "event: {name}\ndata: {}\n\n",
            serde_json::to_string(&value).expect("event")
        )
    };
    vec![
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
}

fn run(cwd: &Path, home: &Path, base_url: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--dangerously-skip-permissions"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
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

fn workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    (dir, home)
}

async fn write_full_stream(socket: &mut TcpStream, events: &[String]) {
    let body = events.concat();
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
}

/// A 429 is retried after the rate-limit floor, not failed to the user.
#[tokio::test(flavor = "multi_thread")]
async fn a_rate_limited_request_is_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(20), listener.accept()).await
            else {
                break;
            };
            let _ = read_body(&mut socket).await;
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                socket
                    .write_all(
                        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write 429");
            } else {
                write_full_stream(&mut socket, &text_events("Retried after the rate limit.")).await;
            }
            let _ = socket.shutdown().await;
        }
    });

    let (dir, home) = workspace();
    let cwd = dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let base = base_url.clone();
    let started = Instant::now();
    let output = tokio::task::spawn_blocking(move || run(&cwd, &home_path, &base))
        .await
        .expect("join");
    let elapsed = started.elapsed();
    server.abort();
    let _ = server.await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "the 429 was not retried"
    );
    assert!(stdout.contains("Retried after the rate limit."), "{stdout}");
    assert!(
        elapsed >= Duration::from_secs(2),
        "the rate-limit backoff floor was skipped ({elapsed:?})"
    );
}

/// A 500 is retried rather than surfaced as an error.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_is_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(20), listener.accept()).await
            else {
                break;
            };
            let _ = read_body(&mut socket).await;
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                socket
                    .write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write 500");
            } else {
                write_full_stream(
                    &mut socket,
                    &text_events("Recovered from the server error."),
                )
                .await;
            }
            let _ = socket.shutdown().await;
        }
    });

    let (dir, home) = workspace();
    let cwd = dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let base = base_url.clone();
    let output = tokio::task::spawn_blocking(move || run(&cwd, &home_path, &base))
        .await
        .expect("join");
    server.abort();
    let _ = server.await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "the 500 was not retried"
    );
    assert!(
        stdout.contains("Recovered from the server error."),
        "{stdout}"
    );
}

/// A stream that arrives slowly still produces the whole answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_stream_completes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
        let Ok(Ok((mut socket, _))) =
            tokio::time::timeout(Duration::from_secs(20), listener.accept()).await
        else {
            return;
        };
        let _ = read_body(&mut socket).await;
        let events = text_events("A slowly delivered answer.");
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .await
            .expect("write headers");
        for event in events {
            tokio::time::sleep(Duration::from_millis(60)).await;
            if socket.write_all(event.as_bytes()).await.is_err() {
                return;
            }
            let _ = socket.flush().await;
        }
        let _ = socket.shutdown().await;
    });

    let (dir, home) = workspace();
    let cwd = dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let base = base_url.clone();
    let started = Instant::now();
    let output = tokio::task::spawn_blocking(move || run(&cwd, &home_path, &base))
        .await
        .expect("join");
    let elapsed = started.elapsed();
    server.abort();
    let _ = server.await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("A slowly delivered answer."), "{stdout}");
    assert!(
        elapsed >= Duration::from_millis(120),
        "the stream was not slow ({elapsed:?})"
    );
}
