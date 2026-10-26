//! `--append-system-prompt` through the real binary: the flag takes a value
//! and the text reaches the provider's system prompt.

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
    let response = serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string();
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                response.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
    socket.shutdown().await.expect("shutdown");
}

fn run_coderex(args: &[&str], cwd: &Path, base_url: &str) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(args)
        .arg("hi")
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

/// Serve every request the session makes (the turn, and the session-title
/// side query) and return their bodies.
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

#[tokio::test]
async fn the_appended_prompt_reaches_the_provider() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let dir = tempfile::tempdir().expect("workspace");

    let server = tokio::spawn(serve_until_idle(listener));
    let args = ["-p", "--append-system-prompt", "APPENDED-MARKER-XYZ"];
    let output = tokio::task::spawn_blocking(move || run_coderex(&args, dir.path(), &base_url))
        .await
        .expect("join");

    let bodies = server.await.expect("server task");
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!bodies.is_empty(), "no provider request was made");
    let text = serde_json::to_string(&bodies).expect("bodies");
    assert!(
        text.contains("APPENDED-MARKER-XYZ"),
        "the appended text is missing from the request: {text}"
    );
}

#[test]
fn the_flag_alone_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--append-system-prompt"])
        .current_dir(dir.path())
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::null())
        .output()
        .expect("run coderex");

    assert!(!output.status.success(), "a value is required");
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("value") || stderr.contains("argument"),
        "{stderr}"
    );
}
