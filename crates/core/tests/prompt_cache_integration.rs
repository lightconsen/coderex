//! Integration tests for prompt caching.
//!
//! The unit tests call `apply_cache_breakpoints` directly; these capture the
//! actual HTTP body the Anthropic client sends, so the four-breakpoint cap
//! and their placement are checked on the wire.

use coderex_core::api_client::{AnthropicApiClient, ApiClient, ApiConfig};
use coderex_core::provider::Provider;
use coderex_core::providers::OpenAiCompatibleProvider;
use coderex_core::tool_executor::ToolInfo;
use coderex_core::types::ModelId;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ─── Capturing server ──────────────────────────────────────────────────────

#[derive(Debug)]
struct Captured {
    body: Value,
}

async fn capture(response: Value) -> (String, tokio::task::JoinHandle<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
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
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buffer.len() < header_end + content_length {
            let mut chunk = vec![0u8; content_length.max(1)];
            let read = socket.read(&mut chunk).await.expect("read body");
            assert!(read > 0, "closed before body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body: Value =
            serde_json::from_slice(&buffer[header_end..header_end + content_length]).expect("json");
        let text = response.to_string();
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                )
                .as_bytes(),
            )
            .await
            .expect("write");
        socket.shutdown().await.expect("shutdown");
        Captured { body }
    });
    (format!("http://{addr}"), handle)
}

fn anthropic_response() -> Value {
    serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

async fn send_anthropic(
    messages: Vec<Value>,
    tools: &[ToolInfo],
    system: &str,
    prompt_caching: bool,
) -> Value {
    let (base_url, handle) = capture(anthropic_response()).await;
    let mut config = ApiConfig::new("test-key".to_string());
    config.base_url = base_url;
    config.prompt_caching = prompt_caching;
    let client = AnthropicApiClient::new(config);
    client
        .create_message(system, &messages, &ModelId::sonnet(), 1024, tools)
        .await
        .expect("request succeeds");
    handle.await.expect("capture").body
}

fn tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "Read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type": "object"}),
        },
        ToolInfo {
            name: "Bash".into(),
            description: "run".into(),
            input_schema: serde_json::json!({"type": "object"}),
        },
    ]
}

fn message(role: &str, text: &str) -> Value {
    serde_json::json!({"role": role, "content": text})
}

fn count_markers(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(count_markers).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_markers).sum(),
        _ => 0,
    }
}

fn last_marked(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.contains_key("cache_control"))
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_long_request_carries_exactly_four_breakpoints() {
    let messages = vec![
        message("user", "one"),
        message("assistant", "two"),
        message("user", "three"),
        message("assistant", "four"),
        message("user", "five"),
    ];

    let body = send_anthropic(messages, &tools(), "you are CodeRex", true).await;

    assert_eq!(count_markers(&body), 4, "{body:#}");
    assert!(
        body["system"]
            .as_array()
            .is_some_and(|blocks| last_marked(&blocks[0])),
        "the system prompt is the first breakpoint: {body:#}"
    );
    assert!(
        last_marked(body["tools"].as_array().unwrap().last().unwrap()),
        "the last tool definition: {body:#}"
    );
    let messages = body["messages"].as_array().unwrap();
    assert!(
        last_marked(&messages[4]["content"][0]),
        "the last message: {body:#}"
    );
    assert!(
        last_marked(&messages[3]["content"][0]),
        "the second-to-last message: {body:#}"
    );
}

#[tokio::test]
async fn without_tools_only_system_and_messages_are_marked() {
    let messages = vec![
        message("user", "one"),
        message("assistant", "two"),
        message("user", "three"),
    ];

    let body = send_anthropic(messages, &[], "system", true).await;

    assert_eq!(count_markers(&body), 3, "{body:#}");
}

#[tokio::test]
async fn a_single_message_gets_system_and_last_message_only() {
    let body = send_anthropic(vec![message("user", "only")], &[], "system", true).await;

    assert_eq!(count_markers(&body), 2, "{body:#}");
}

#[tokio::test]
async fn prompt_caching_disabled_sends_no_markers() {
    let messages = vec![message("user", "one"), message("assistant", "two")];

    let body = send_anthropic(messages, &tools(), "system", false).await;

    assert_eq!(count_markers(&body), 0, "{body:#}");
    assert_eq!(body["system"], "system", "the string form is preserved");
}

#[tokio::test]
async fn an_empty_system_prompt_is_not_marked() {
    let body = send_anthropic(vec![message("user", "one")], &[], "", true).await;

    assert_eq!(count_markers(&body), 1, "{body:#}");
    assert_eq!(body["system"], "");
}

#[tokio::test]
async fn the_openai_protocol_never_receives_cache_control() {
    let (base_url, handle) = capture(serde_json::json!({
        "id": "chatcmpl_test",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .await;
    let provider = OpenAiCompatibleProvider::new("openai", base_url, "test-key");

    provider
        .create_message(
            "system",
            &[message("user", "one"), message("assistant", "two")],
            &ModelId::sonnet(),
            1024,
            &tools(),
        )
        .await
        .expect("request succeeds");

    let body = handle.await.expect("capture").body;
    assert_eq!(count_markers(&body), 0, "{body:#}");
}
