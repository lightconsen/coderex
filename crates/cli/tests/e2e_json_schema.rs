//! `--json-schema` through the real binary: the schema instruction reaches the
//! provider request. Whether the model obeys it is model behavior, not a
//! property this client can guarantee, so the contract asserted here is the
//! instruction itself.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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

async fn respond(socket: &mut TcpStream) {
    // The default headless path streams, so the answer has to be an SSE body,
    // not a single JSON message.
    let answer = "{\"greeting\":\"hello\"}";
    let event = |name: &str, value: serde_json::Value| {
        format!(
            "event: {name}\ndata: {}\n\n",
            serde_json::to_string(&value).expect("event json")
        )
    };
    let body = [
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
                "delta": {"type": "text_delta", "text": answer},
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
    .concat();
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
    socket.shutdown().await.expect("shutdown");
}

async fn serve_until_idle(listener: tokio::net::TcpListener) -> Vec<serde_json::Value> {
    let mut bodies = Vec::new();
    let mut idle = Duration::from_secs(20);
    loop {
        let Ok(Ok((mut socket, _))) = tokio::time::timeout(idle, listener.accept()).await else {
            break;
        };
        bodies.push(read_body(&mut socket).await);
        respond(&mut socket).await;
        idle = Duration::from_secs(3);
    }
    bodies
}

fn run(cwd: &Path, base_url: &str) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--json-schema", "schema.json", "hello"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "test")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run coderex")
}

#[tokio::test]
async fn the_schema_instruction_reaches_the_provider_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(
        dir.path().join("schema.json"),
        r#"{"type":"object","properties":{"greeting":{"type":"string"}},"required":["greeting"]}"#,
    )
    .expect("write schema");

    let server = tokio::spawn(serve_until_idle(listener));
    let output = tokio::task::spawn_blocking(move || run(dir.path(), &base_url))
        .await
        .expect("join");
    let bodies = server.await.expect("server task");
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let turn = bodies
        .iter()
        .find(|body| {
            body["messages"]
                .as_array()
                .is_some_and(|messages| !messages.is_empty())
        })
        .expect("a request was made");
    let text = serde_json::to_string(turn).expect("body");
    assert!(
        text.contains("ONLY valid JSON"),
        "the structured-output instruction is missing: {text}"
    );
    assert!(
        text.contains("greeting"),
        "the schema itself is missing: {text}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "{\"greeting\":\"hello\"}",
        "the answer is passed through"
    );
}
