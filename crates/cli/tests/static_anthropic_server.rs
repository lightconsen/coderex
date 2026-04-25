//! Static Anthropic Server integration tests — verifying the binary starts,
//! loads recorded responses, and serves them correctly.
//!
//! These tests correspond to the equivalence test plan:
//! E2E mock server with real LLM responses for deterministic testing.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Find a free TCP port by binding to 127.0.0.1:0 and reading the assigned port.
fn find_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to find free port");
    listener.local_addr().unwrap().port()
}

/// Start the static-anthropic-server binary on a free port.
fn start_server(port: u16) -> Child {
    let responses_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/static_responses.json");

    assert!(
        responses_path.exists(),
        "responses file not found: {}",
        responses_path.display()
    );

    // Prefer pre-built binary to avoid cargo compilation overhead in tests
    let binary_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/static-anthropic-server");

    let child = if binary_path.exists() {
        Command::new(&binary_path)
            .args([
                "--port",
                &port.to_string(),
                "--responses",
                responses_path.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    } else {
        Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "--bin",
                "static-anthropic-server",
                "--",
                "--port",
                &port.to_string(),
                "--responses",
                responses_path.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
    .expect("failed to spawn static-anthropic-server");

    child
}

/// Wait for the server to be accepting connections.
async fn wait_for_server(port: u16, max_attempts: u32) {
    for _ in 0..max_attempts {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("server did not start within {} attempts", max_attempts);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn static_server_loads_responses_and_serves_tool_use() {
    let port = find_free_port();
    let mut child = start_server(port);

    let addr = format!("http://127.0.0.1:{port}/v1/messages");
    wait_for_server(port, 50).await;

    // Send a prompt that triggers a tool_use response (fibonacci -> Write)
    let body = serde_json::json!({
        "model": "K2.6",
        "messages": [{"role": "user", "content": "Add a recursive fibonacci function to src/lib.rs with unit tests."}],
        "max_tokens": 1024,
        "stream": true,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&addr)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // Should contain a Write tool_use block
    assert!(
        text.contains("tool_use"),
        "expected tool_use in response, got: {text}"
    );
    assert!(text.contains("Write"), "expected Write tool, got: {text}");
    assert!(
        text.contains("src/lib.rs"),
        "expected file path in tool input"
    );

    let _ = child.kill();
}

#[tokio::test]
async fn static_server_serves_recorded_text_after_tool_result() {
    let port = find_free_port();
    let mut child = start_server(port);

    let addr = format!("http://127.0.0.1:{port}/v1/messages");
    wait_for_server(port, 50).await;

    // Send a prompt that simulates a tool_result already in history
    let body = serde_json::json!({
        "model": "K2.6",
        "messages": [
            {"role": "user", "content": "Add a recursive fibonacci function to src/lib.rs with unit tests."},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "tool_01", "name": "Write", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_01", "content": "File written successfully."}]}
        ],
        "max_tokens": 1024,
        "stream": true,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&addr)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // Should contain the recorded static text response, not a hardcoded "Done!"
    assert!(
        text.contains("text_delta"),
        "expected text_delta in response, got: {text}"
    );
    // The recorded fibonacci text is much longer than the old 42-char placeholder
    let text_content = extract_text_from_sse(&text);
    assert!(
        text_content.len() > 100,
        "expected recorded LLM text (>100 chars), got only {} chars: {text_content}",
        text_content.len()
    );

    let _ = child.kill();
}

#[tokio::test]
async fn static_server_returns_404_for_non_messages_endpoint() {
    let port = find_free_port();
    let mut child = start_server(port);

    let addr = format!("http://127.0.0.1:{port}/v1/models");
    wait_for_server(port, 50).await;

    let resp = reqwest::get(&addr).await.expect("request failed");
    assert_eq!(resp.status(), 404);

    let _ = child.kill();
}

#[tokio::test]
async fn static_server_json_schema_mode_returns_structured_output() {
    let port = find_free_port();
    let mut child = start_server(port);

    let addr = format!("http://127.0.0.1:{port}/v1/messages");
    wait_for_server(port, 50).await;

    // Send a prompt that includes a json_schema in the system prompt
    let body = serde_json::json!({
        "model": "K2.6",
        "system": "You must respond with JSON matching this schema: {\"type\":\"object\",\"properties\":{\"greeting\":{\"type\":\"string\"}}}",
        "messages": [{"role": "user", "content": "Say hello"}],
        "max_tokens": 1024,
        "stream": true,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&addr)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("greeting"),
        "expected greeting in structured output, got: {text}"
    );

    let _ = child.kill();
}

#[tokio::test]
async fn static_server_simulates_error_for_error_trigger_prompt() {
    let port = find_free_port();
    let mut child = start_server(port);

    let addr = format!("http://127.0.0.1:{port}/v1/messages");
    wait_for_server(port, 50).await;

    let body = serde_json::json!({
        "model": "K2.6",
        "messages": [{"role": "user", "content": "simulate error please"}],
        "max_tokens": 1024,
        "stream": true,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&addr)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 500);

    let _ = child.kill();
}

#[tokio::test]
async fn static_server_non_streaming_mode_returns_json() {
    let port = find_free_port();
    let mut child = start_server(port);

    let addr = format!("http://127.0.0.1:{port}/v1/messages");
    wait_for_server(port, 50).await;

    let body = serde_json::json!({
        "model": "K2.6",
        "messages": [{"role": "user", "content": "Hello! Can you help me with coding?"}],
        "max_tokens": 1024,
        "stream": false,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&addr)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json.get("type").unwrap(), "message");
    assert_eq!(json.get("role").unwrap(), "assistant");
    assert!(json.get("content").is_some());

    let _ = child.kill();
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// Extract text content from SSE response by concatenating text_delta events.
fn extract_text_from_sse(sse: &str) -> String {
    let mut result = String::new();
    for line in sse.lines() {
        if line.starts_with("data: ") {
            let json_str = line.trim_start_matches("data: ");
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(text) = value
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    result.push_str(text);
                }
            }
        }
    }
    result
}
