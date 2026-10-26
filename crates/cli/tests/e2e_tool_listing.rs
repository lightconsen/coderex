//! The tool list the model receives, captured from the real binary's request.
//!
//! A tool the model is never told about cannot be used; one it is told about
//! but that will refuse is just noise. `--allowed-tools` is the user-visible
//! switch, and the request body is the only place the result is observable.

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

fn run(cwd: &Path, base_url: &str, args: &[&str]) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    Command::new(env!("CARGO_BIN_EXE_coderex"))
        .arg("-p")
        .args(args)
        .arg("hello")
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

async fn tool_names(args: &[&str]) -> Vec<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let dir = tempfile::tempdir().expect("workspace");

    let server = tokio::spawn(serve_until_idle(listener));
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let output = tokio::task::spawn_blocking(move || {
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        run(dir.path(), &base_url, &refs)
    })
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
            body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        })
        .expect("the turn request advertises tools");
    turn["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn chrome_tools_follow_the_session_flag() {
    let enabled = tool_names(&["--dangerously-skip-permissions", "--chrome"]).await;
    assert!(
        enabled.iter().any(|name| name == "chrome_navigate"),
        "--chrome did not advertise its tools: {enabled:?}"
    );

    let disabled = tool_names(&["--dangerously-skip-permissions", "--chrome", "--no-chrome"]).await;
    assert!(
        !disabled.iter().any(|name| name.starts_with("chrome_")),
        "--no-chrome left Chrome tools visible: {disabled:?}"
    );
}

#[tokio::test]
async fn the_default_listing_includes_baseline_tools() {
    let names = tool_names(&["--dangerously-skip-permissions"]).await;

    for expected in ["Read", "Write", "Bash", "LSP"] {
        assert!(
            names.iter().any(|name| name == expected),
            "{expected} is missing from the model-visible tools: {names:?}"
        );
    }
}
