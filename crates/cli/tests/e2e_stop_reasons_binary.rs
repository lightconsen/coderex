//! Provider stop reasons and retry classification through the real binary.
//!
//! The engine's stop-reason branches (`pause_turn`, `max_tokens`, `refusal`)
//! were only ever exercised through a mock `ApiClient`. Here the static
//! provider answers them over HTTP/SSE, so the branch runs with the real
//! transport, the real engine loop, and the real headless output path. The
//! retry test counts the requests that reach the provider: a failure that is
//! not retryable must be sent exactly once.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Start the static provider.
fn start_static_provider() -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let args = vec![
        "--port".to_string(),
        "0".to_string(),
        "--responses".to_string(),
        responses.to_str().expect("responses path").to_string(),
    ];

    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args(&args)
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

fn run_headless(cwd: &Path, base_url: &str, prompt: &str) -> std::process::Output {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--dangerously-skip-permissions"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
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
        .write_all(format!("{prompt}\n").as_bytes())
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

#[test]
fn a_paused_turn_is_re_sent_and_the_resumed_answer_is_reported() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();

    let output = run_headless(dir.path(), &base_url, "pause-turn-once please");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    // The run reports the answer the *resumed* request produced, not the
    // partial text the paused one delivered.
    assert!(
        stdout.contains("Resumed after the provider pause."),
        "the paused turn was not resumed:\n{stdout}"
    );
    // The resumed run ends on `Completed`, so nothing is appended to the
    // answer. A `Resume` reaching the output would mean the loop reported the
    // pause as the end of the run.
    assert!(
        !stdout.contains("Query completed"),
        "the pause was reported as a finished turn:\n{stdout}"
    );
}

#[test]
fn an_answer_cut_off_by_the_output_limit_says_so() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();

    let output = run_headless(dir.path(), &base_url, "run out of output room");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("The answer was cut off"), "{stdout}");
    assert!(
        stdout.contains("TokenLimitStop"),
        "a truncated answer was reported as a finished one:\n{stdout}"
    );
}

#[test]
fn a_refusal_is_a_finished_turn_not_an_error() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();

    let output = run_headless(dir.path(), &base_url, "decline-this-request now");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert_eq!(
        stdout.trim(),
        "I can't help with that.",
        "a refusal was decorated with an internal reason:\n{stdout}"
    );
}

// ── Retry classification ───────────────────────────────────────────────────

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

/// A `400` is the provider's verdict on the request itself; retrying it would
/// only repeat it. Exactly one streaming request may reach the provider.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_retryable_failure_is_requested_only_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let streaming = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_streaming = std::sync::Arc::clone(&streaming);
    let seen_total = std::sync::Arc::clone(&total);
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(20), listener.accept()).await
            else {
                break;
            };
            let body = read_body(&mut socket).await;
            seen_total.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if body["stream"].as_bool().unwrap_or(false) {
                seen_streaming.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let message = r#"{"type":"error","error":{"type":"invalid_request_error","message":"malformed request"}}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{message}",
                message.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

    let dir = tempfile::tempdir().expect("workspace");
    let cwd = dir.path().to_path_buf();
    let base = base_url.clone();
    let output = tokio::task::spawn_blocking(move || run_headless(&cwd, &base, "hello there"))
        .await
        .expect("join");
    server.abort();
    let _ = server.await;

    assert_eq!(
        streaming.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a 400 was retried: {} requests in total reached the provider",
        total.load(std::sync::atomic::Ordering::SeqCst)
    );
    // A failed stream is re-asked once as a plain request (streaming cannot be
    // resumed at an offset), and that second request is refused too. The side
    // request for a session title is the only other traffic.
    assert_eq!(
        total.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "a non-retryable failure multiplied beyond the one streaming request, \
         its non-streaming re-ask, and the title request"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("malformed request") || stderr.contains("malformed request"),
        "the provider's message was lost:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// A retryable failure is retried: the same request goes out more than once
/// and the second answer completes the turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_retryable_failure_is_requested_again() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let streaming = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&streaming);
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(20), listener.accept()).await
            else {
                break;
            };
            let body = read_body(&mut socket).await;
            let is_streaming = body["stream"].as_bool().unwrap_or(false);
            if !is_streaming {
                // The title side request answers normally throughout.
                let text = r#"{"title":"Retry session"}"#;
                let message = format!(
                    r#"{{"id":"msg_title","type":"message","role":"assistant","content":[{{"type":"text","text":{}}}],"stop_reason":"end_turn","stop_sequence":null,"usage":{{"input_tokens":1,"output_tokens":1}}}}"#,
                    serde_json::to_string(text).expect("json")
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{message}",
                    message.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                continue;
            }
            if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
            } else {
                let events = text_stream("Answer after the retry.");
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{events}",
                            events.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .ok();
            }
            let _ = socket.shutdown().await;
        }
    });

    let dir = tempfile::tempdir().expect("workspace");
    let cwd = dir.path().to_path_buf();
    let base = base_url.clone();
    let output = tokio::task::spawn_blocking(move || run_headless(&cwd, &base, "hello there"))
        .await
        .expect("join");
    server.abort();
    let _ = server.await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        streaming.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "the 503 was not retried"
    );
    assert!(
        stdout.contains("Answer after the retry."),
        "the retried answer was lost:\n{stdout}"
    );
}

fn text_stream(text: &str) -> String {
    let delta = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": text},
    });
    format!(
        "event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {delta}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}
