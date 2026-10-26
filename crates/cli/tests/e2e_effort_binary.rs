//! `--effort` through the real binary against an OpenAI-shaped provider: the
//! request carries `reasoning_effort`, and the answer is passed through.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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

fn chunk(delta: &serde_json::Value, finish: Option<&str>) -> String {
    let value = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!(
        "data: {}\n\n",
        serde_json::to_string(&value).expect("chunk")
    )
}

#[tokio::test]
async fn effort_reaches_an_openai_shaped_provider() {
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
            let body = read_request(&mut socket).await;
            seen.lock().unwrap().push(body);
            let sse = [
                chunk(
                    &serde_json::json!({"role": "assistant", "content": "effort answer"}),
                    None,
                ),
                chunk(&serde_json::json!({}), Some("stop")),
                "data: [DONE]\n\n".to_string(),
            ]
            .concat();
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
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let cwd = dir.path().to_path_buf();
    let base_for_child = base_url.clone();
    let home = config_home.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || run(&cwd, &base_for_child, &home, "high"))
        .await
        .expect("join");
    server.abort();
    let _ = server.await;

    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bodies = bodies.lock().unwrap().clone();
    assert!(!bodies.is_empty(), "no provider request was made");
    let main = bodies
        .iter()
        .find(|body| body["max_tokens"] == 4096)
        .expect("the turn request is present");
    assert_eq!(
        main["reasoning_effort"], "high",
        "reasoning_effort was not stamped on the turn: {main:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("effort answer"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn an_unsupported_effort_is_rejected_by_the_cli() {
    let dir = tempfile::tempdir().expect("workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--effort", "extreme", "hello"])
        .current_dir(dir.path())
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::null())
        .output()
        .expect("run coderex");
    assert!(!output.status.success(), "an unknown effort must fail");
}

fn run(cwd: &Path, base_url: &str, config_home: &Path, effort: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--model", "gpt-4o", "--effort", effort])
        .current_dir(cwd)
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", base_url)
        .env("OPENAI_MODEL", "gpt-4o")
        // The endpoint is a local capture server, so name the profile
        // explicitly: a localhost base URL otherwise resolves to "custom",
        // which correctly reports no reasoning-effort control.
        .env("CODEREX_PROVIDER_PROFILE", "openai")
        // The developer's shell may export an Anthropic endpoint; the test is
        // about the OpenAI-shaped provider.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("ANTHROPIC_MODEL")
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
