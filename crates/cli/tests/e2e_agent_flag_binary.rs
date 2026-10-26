//! `--agent` through the real binary: a named custom agent supplies the
//! session's system prompt and tool whitelist, and an unknown name fails with
//! the configured list.

use std::io::Write;
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

async fn serve_until_idle(listener: tokio::net::TcpListener) -> Vec<serde_json::Value> {
    let mut bodies = Vec::new();
    let mut idle = Duration::from_secs(20);
    loop {
        let Ok(Ok((mut socket, _))) = tokio::time::timeout(idle, listener.accept()).await else {
            break;
        };
        bodies.push(read_body(&mut socket).await);
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
        idle = Duration::from_secs(3);
    }
    bodies
}

fn seed_agent(home: &Path, name: &str, prompt: &str) {
    let agents = home.join(".coderex").join("agents");
    std::fs::create_dir_all(&agents).expect("agents dir");
    std::fs::write(
        agents.join(format!("{name}.json")),
        serde_json::json!({
            "name": name,
            "system_prompt": prompt,
            "allowed_tools": ["Read"]
        })
        .to_string(),
    )
    .expect("agent definition");
    std::fs::write(
        home.join(".coderex").join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
}

fn run(
    home: &Path,
    cwd: &Path,
    base_url: &str,
    prompt: &str,
    args: &[&str],
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .arg("-p")
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "test")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("HOME", home)
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

#[tokio::test]
async fn the_named_agent_sets_the_prompt_and_tool_whitelist() {
    let home = tempfile::tempdir().expect("home");
    seed_agent(home.path(), "reviewer", "AGENT-MARKER-XYZ");
    let cwd = tempfile::tempdir().expect("workspace");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));

    let server = tokio::spawn(serve_until_idle(listener));
    std::fs::create_dir_all(cwd.path().join("src")).unwrap();
    std::fs::write(
        cwd.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let home_path = home.path().to_path_buf();
    let cwd_path = cwd.path().to_path_buf();
    let base = base_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        run(
            &home_path,
            &cwd_path,
            &base,
            "add a recursive fibonacci function to src/lib.rs with unit tests",
            &["--agent", "reviewer"],
        )
    })
    .await
    .expect("join");
    let bodies = server.await.expect("server");

    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let turn = bodies
        .iter()
        .find(|body| body["max_tokens"] == 4096)
        .expect("the turn request");
    let text = serde_json::to_string(turn).expect("body");
    assert!(
        text.contains("AGENT-MARKER-XYZ"),
        "the agent system prompt is missing: {text}"
    );
    // The whitelist restricts execution: the Write the model asks for is
    // refused, so the fixture keeps its original content.
    let content = std::fs::read_to_string(cwd.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "the agent whitelist let Write run: {content}"
    );
}

#[test]
fn an_unknown_agent_lists_the_configured_names() {
    let home = tempfile::tempdir().expect("home");
    seed_agent(home.path(), "reviewer", "You review.");
    let cwd = tempfile::tempdir().expect("workspace");

    let output = run(
        home.path(),
        cwd.path(),
        "http://127.0.0.1:9",
        "hello",
        &["--agent", "missing"],
    );

    assert!(!output.status.success(), "an unknown agent must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown agent"), "{stderr}");
    assert!(stderr.contains("reviewer"), "{stderr}");
}
