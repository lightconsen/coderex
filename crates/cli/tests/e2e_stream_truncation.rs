//! A provider stream that dies mid-answer: the session must recover the
//! delivered prefix and finish the answer from the retried stream.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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

fn sse_text(text: &str) -> String {
    let event = |name: &str, value: serde_json::Value| {
        format!(
            "event: {name}\ndata: {}\n\n",
            serde_json::to_string(&value).expect("event")
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

#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_stream_is_retried_and_the_prefix_kept() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(15), listener.accept()).await
            else {
                break;
            };
            let _body = read_body(&mut socket).await;
            let attempt = seen.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                // Deliver a prefix, then die without message_stop.
                let partial = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                               event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"The answer is \"}}\n\n";
                socket
                    .write_all(
                        format!(
                            // A chunked response that never gets its
                            // terminating chunk: the connection dies mid-body.
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{partial}\r\n",
                            partial.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write partial");
            } else {
                let sse = sse_text("The answer is 42.");
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                            sse.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write retry");
            }
            let _ = socket.shutdown().await;
        }
    });

    let dir = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let cwd = dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let base = base_url.clone();
    let output = tokio::task::spawn_blocking(move || run(&cwd, &home_path, &base))
        .await
        .expect("join");
    server.abort();
    let _ = server.await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "coderex failed: {stderr}");
    assert!(
        requests.load(Ordering::SeqCst) >= 2,
        "the truncated stream was not retried (requests={}); stdout: {stdout}",
        requests.load(Ordering::SeqCst)
    );
    assert!(
        stdout.contains("The answer is 42."),
        "the retried answer is missing: {stdout}"
    );
    assert!(
        !stdout.contains("The answer is The answer is"),
        "the delivered prefix was duplicated: {stdout}"
    );
}
