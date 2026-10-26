//! OpenAI-compatible provider against a fake HTTP server.
//!
//! Ported from qwen-code's `fake-openai-server` suite, but scoped to the
//! behaviors CodeRex's client must understand: non-streaming content and
//! reasoning, streaming reasoning/content/tool-call deltas, and the in-band
//! `error_finish` failure. The fake server writes one response and closes the
//! connection, so no port needs to be guessed.

use coderex_core::api_client::ApiError;
use coderex_core::api_client::StreamEvent;
use coderex_core::provider::Provider;
use coderex_core::providers::OpenAiCompatibleProvider;
use coderex_core::types::ModelId;
use futures::StreamExt;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Start a server that accepts one request and answers with `body`.
///
/// The response always uses `Connection: close`; the client reads the body
/// until EOF, which works for both JSON and SSE.
async fn serve_once(
    body: String,
    content_type: &'static str,
) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 8192];
        let header_end = loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let content_length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        while buffer.len() < header_end + content_length {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(
            &buffer[header_end..(header_end + content_length).min(buffer.len())],
        ) {
            recorded.lock().expect("requests").push(value);
        }

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n{body}"
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });

    (format!("http://{address}"), requests)
}

fn user_message() -> Vec<serde_json::Value> {
    vec![json!({"role": "user", "content": "hello"})]
}

#[tokio::test]
async fn non_streaming_content_and_reasoning_are_parsed() {
    let response = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "fake-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "hello from fake model",
                "reasoning_content": "thinking hard"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
    })
    .to_string();
    let (base_url, requests) = serve_once(response, "application/json").await;

    let provider = OpenAiCompatibleProvider::new("fake", base_url, "key");
    let response = provider
        .create_message(
            "system",
            &user_message(),
            &ModelId("fake-model".into()),
            64,
            &[],
        )
        .await
        .expect("completion");

    assert_eq!(response.content, "hello from fake model");
    assert_eq!(
        response
            .thinking
            .as_ref()
            .map(|block| block.thinking.as_str()),
        Some("thinking hard")
    );
    assert_eq!(response.stop_reason, "stop");
    assert_eq!(response.usage.input_tokens, 3);
    assert_eq!(response.usage.output_tokens, 4);

    let recorded = requests.lock().expect("requests");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["model"], "fake-model");
    // The body omits `stream` when it is false.
    assert_ne!(recorded[0]["stream"], serde_json::Value::Bool(true));
}

#[tokio::test]
async fn streaming_reasoning_content_and_tool_calls_keep_their_order() {
    let sse = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think \"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hard\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"Write\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"x\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6,\"total_tokens\":11}}\n\n",
        "data: [DONE]\n\n"
    )
    .to_string();
    let (base_url, _requests) = serve_once(sse, "text/event-stream").await;

    let provider = OpenAiCompatibleProvider::new("fake", base_url, "key");
    let events: Vec<StreamEvent> = provider
        .create_message_streaming(
            "system",
            &user_message(),
            &ModelId("fake-model".into()),
            64,
            &[],
        )
        .await
        .collect()
        .await;

    let thinking: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ThinkingDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, ["think ", "hard"]);

    let text: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, ["hel", "lo"]);

    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolCallBlockStart { id, name } if id == "call_1" && name == "Write"
    )));
    // OpenAI streams the arguments as deltas; the block end carries no parsed
    // input, the engine's assembler joins the deltas.
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolCallInputDelta { id, delta } if id == "call_1" && delta.contains("\"path\":\"x\"")
    )));
    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::ToolCallBlockEnd { id, .. } if id == "call_1")
        )
    );
    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::StopReason(reason) if reason == "tool_calls")
        )
    );
    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::UsageSummary(usage) if usage.input_tokens == 5)
        )
    );

    // Reasoning must precede the visible text in the event stream.
    let first_text = events
        .iter()
        .position(|event| matches!(event, StreamEvent::TextDelta(_)))
        .expect("text delta");
    let last_thinking = events
        .iter()
        .rposition(|event| matches!(event, StreamEvent::ThinkingDelta(_)))
        .expect("thinking delta");
    assert!(last_thinking < first_text);
}

#[tokio::test]
async fn an_error_finish_chunk_becomes_a_stream_error_not_text() {
    let sse = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fake-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"boom\"},\"finish_reason\":\"error_finish\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_string();
    let (base_url, _requests) = serve_once(sse, "text/event-stream").await;

    let provider = OpenAiCompatibleProvider::new("fake", base_url, "key");
    let events: Vec<StreamEvent> = provider
        .create_message_streaming(
            "system",
            &user_message(),
            &ModelId("fake-model".into()),
            64,
            &[],
        )
        .await
        .collect()
        .await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error(ApiError::Http(message)) if message == "boom")),
        "expected the in-band error text to surface as a stream error: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "boom")),
        "error text must not be rendered as answer text"
    );
}
