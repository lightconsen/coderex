//! Automatic compaction through the real binary: a tiny context window makes
//! the pre-request threshold check run the LLM summary before the turn.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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

async fn run_case(window: Option<&str>, prompt: &str) -> (bool, String, Vec<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let systems = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = Arc::clone(&systems);
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(10), listener.accept()).await
            else {
                break;
            };
            let body = read_body(&mut socket).await;
            seen.lock().unwrap().push(body);
            let sse = sse_text("ok");
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                        sse.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("write");
            socket.shutdown().await.expect("shutdown");
        }
    });

    let dir = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let cwd = dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let base = base_url.clone();
    let window = window.map(str::to_string);
    let prompt = prompt.to_string();
    let output = tokio::task::spawn_blocking(move || {
        run(&cwd, &home_path, &base, window.as_deref(), &prompt)
    })
    .await
    .expect("join");
    server.abort();
    let _ = server.await;
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bodies = systems.lock().unwrap().clone();
    let turn_sent = bodies.iter().any(|body| {
        body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    });
    let systems = bodies
        .iter()
        .map(|body| serde_json::to_string(&body["system"]).unwrap_or_default())
        .collect();
    (
        turn_sent,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        systems,
    )
}

fn run(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    window: Option<&str>,
    prompt: &str,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coderex"));
    command
        .args(["-p", "--dangerously-skip-permissions"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match window {
        Some(window) => {
            command.env("CODEREX_CONTEXT_WINDOW", window);
        }
        None => {
            command.env_remove("CODEREX_CONTEXT_WINDOW");
        }
    }
    let mut child = command.spawn().expect("spawn coderex");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{prompt}\n").as_bytes())
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

#[tokio::test]
async fn a_tiny_context_window_refuses_the_request_with_a_reason() {
    let (normal_turn, normal_stdout, _) = run_case(None, "hello").await;
    assert!(normal_turn, "an ordinary turn did not reach the provider");
    assert!(normal_stdout.contains("ok"), "{normal_stdout}");

    let (tiny_turn, tiny_stdout, tiny_systems) = run_case(Some("1000"), "hello").await;
    assert!(
        !tiny_turn,
        "a blocked context still sent the turn: {tiny_systems:?}"
    );
    assert!(
        tiny_stdout.contains("context window is still full after compaction"),
        "the refusal did not explain itself: {tiny_stdout}"
    );
}

// ── Summary request and its fallback ───────────────────────────────────────

/// A provider that records every request body and answers with a summary or an
/// error, decided by the caller.
#[allow(clippy::too_many_arguments)]
async fn run_compaction_case(
    window: &str,
    prompt: &str,
    summary_answer: Option<&'static str>,
) -> (Vec<serde_json::Value>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = Arc::clone(&bodies);
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(10), listener.accept()).await
            else {
                break;
            };
            let body = read_body(&mut socket).await;
            // The summarizer's own request. Everything else is the turn.
            let is_summary = body["system"]
                .to_string()
                .contains("handoff summary for another LLM");
            seen.lock().unwrap().push(body);
            let response = if is_summary {
                match summary_answer {
                    Some(text) => format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        sse_text(text).len(),
                        sse_text(text)
                    ),
                    None => {
                        "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_string()
                    }
                }
            } else {
                let sse = sse_text("ok");
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                    sse.len()
                )
            };
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

    let dir = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let cwd = dir.path().to_path_buf();
    let home_path = home.path().to_path_buf();
    let base = base_url.clone();
    let window = window.to_string();
    let prompt = prompt.to_string();
    let output =
        tokio::task::spawn_blocking(move || run(&cwd, &home_path, &base, Some(&window), &prompt))
            .await
            .expect("join");
    server.abort();
    let _ = server.await;
    let bodies = bodies.lock().unwrap().clone();
    (bodies, String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The system prompt reaches the provider as a block array with at most one
/// breakpoint — the shape the API accepts — and the turn's own text is not
/// mixed into the summary request's system prompt.
#[tokio::test]
async fn the_system_prompt_is_one_marked_text_block() {
    let (bodies, _stdout) = run_compaction_case("200000", "hello", Some("unused")).await;
    let turn = bodies
        .iter()
        .find(|body| {
            body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        })
        .expect("no turn request");
    let system = &turn["system"];
    let blocks = system
        .as_array()
        .unwrap_or_else(|| panic!("system is not a block array: {system}"));
    assert!(!blocks.is_empty(), "empty system array");
    for block in blocks {
        assert_eq!(
            block["type"].as_str(),
            Some("text"),
            "a non-text system block: {block}"
        );
    }
    let marked = blocks
        .iter()
        .filter(|block| block.get("cache_control").is_some())
        .count();
    assert_eq!(marked, 1, "the system prompt got {marked} breakpoints");
}
