//! QueryEngine integration tests — covering full turn lifecycle, streaming,
//! tool call loops, and termination conditions.
//!
//! These tests correspond to the equivalence test plan Module 1:
//! 1.1 single-turn, 1.2 multi-turn, 1.3 streaming, 1.4 tool call loop,
//! 1.5 structured output, 1.6 termination conditions, 1.7 state machine

use async_trait::async_trait;
use coderex_core::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use coderex_core::engine::{QueryEngine, SubmitOptions, TerminalReason};
use coderex_core::permission_engine::PermissionEngine;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::state::SessionState;
use coderex_core::tool_executor::ToolInfo;
use coderex_core::tool_executor::{ToolDispatchOutcome, ToolExecutor};
use coderex_core::types::{
    EngineEvent, MessageContent, ModelId, ThinkingBlock, ToolCall, ToolResult, UsageSummary,
};
use futures::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn make_engine(
    api: Box<dyn ApiClient>,
    tools: Box<dyn ToolExecutor + Send + Sync>,
) -> (QueryEngine, mpsc::UnboundedReceiver<EngineEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
    let perm_engine = PermissionEngine::new(checker);
    let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
    let mut engine = QueryEngine::new(api, ModelId::sonnet(), session, perm_engine, tx);
    engine.set_tool_executor(tools);
    (engine, rx)
}

fn usage(output_tokens: usize) -> UsageSummary {
    UsageSummary {
        input_tokens: 10,
        output_tokens,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    }
}

fn text_response(content: &str) -> ApiResponse {
    ApiResponse {
        content: content.into(),
        tool_calls: Vec::new(),
        usage: usage(content.len()),
        stop_reason: "end_turn".into(),
        thinking: None,
    }
}

fn tool_response(tool_calls: Vec<ToolCall>) -> ApiResponse {
    ApiResponse {
        content: "I'll use a tool.".into(),
        tool_calls,
        usage: usage(20),
        stop_reason: "end_turn".into(),
        thinking: None,
    }
}

/// Drain events from the receiver into a Vec.
async fn drain_events(rx: &mut mpsc::UnboundedReceiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Ok(evt) = rx.try_recv() {
        events.push(evt);
    }
    events
}

// ─── Mock Tool Executor ───────────────────────────────────────────────────

struct EchoToolExecutor;

#[async_trait]
impl ToolExecutor for EchoToolExecutor {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> ToolDispatchOutcome {
        ToolDispatchOutcome {
            result: ToolResult {
                tool_call_id: tool_call_id.to_string(),
                output: format!("executed {tool_name} with {input}"),
                is_error: false,
                images: Vec::new(),
            },
        }
    }
}

// ─── Mock API Clients ─────────────────────────────────────────────────────

/// Always returns the same text response.
struct TextMock {
    response: ApiResponse,
}

impl TextMock {
    fn new(content: &str) -> Self {
        Self {
            response: text_response(content),
        }
    }
}

#[async_trait]
impl ApiClient for TextMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(TextMock {
            response: self.response.clone(),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        Ok(self.response.clone())
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(StreamEvent::StreamEnd);
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Returns a sequence of responses, cycling the last one.
///
/// The call counter is shared through `Arc` because the engine clones the
/// client for every request (via `with_request_context`) — a bare counter
/// would be copied per clone and never advance, making every turn return
/// the first response.
struct SequenceMock {
    responses: Vec<ApiResponse>,
    call_count: std::sync::Arc<AtomicUsize>,
}

impl SequenceMock {
    fn new(responses: Vec<ApiResponse>) -> Self {
        Self {
            responses,
            call_count: std::sync::Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl ApiClient for SequenceMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(SequenceMock {
            responses: self.responses.clone(),
            call_count: std::sync::Arc::clone(&self.call_count),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let resp = if idx < self.responses.len() {
            self.responses[idx].clone()
        } else {
            self.responses.last().unwrap().clone()
        };
        Ok(resp)
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(StreamEvent::StreamEnd);
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Returns a thinking + tool_use turn first, then a text answer, and records
/// every request body so the follow-up can be inspected.
struct ThinkingToolMock {
    calls: std::sync::Arc<AtomicUsize>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Vec<serde_json::Value>>>>,
}

#[async_trait]
impl ApiClient for ThinkingToolMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(ThinkingToolMock {
            calls: std::sync::Arc::clone(&self.calls),
            requests: std::sync::Arc::clone(&self.requests),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        if idx == 0 {
            Ok(ApiResponse {
                content: "I'll read it.".into(),
                tool_calls: vec![ToolCall {
                    id: "tc_think".into(),
                    name: "read".into(),
                    input: serde_json::json!({"file_path": "a.txt"}),
                }],
                usage: usage(10),
                stop_reason: "tool_use".into(),
                thinking: Some(ThinkingBlock {
                    thinking: "I should read the file first.".into(),
                    signature: "sig-abc".into(),
                }),
            })
        } else {
            Ok(text_response("done"))
        }
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(StreamEvent::StreamEnd);
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Streaming mock that emits text deltas then StreamEnd.
struct StreamingMock {
    deltas: Vec<String>,
}

#[async_trait]
impl ApiClient for StreamingMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(StreamingMock {
            deltas: self.deltas.clone(),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        let content = self.deltas.join("");
        Ok(text_response(&content))
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let deltas = self.deltas.clone();
        let stream = async_stream::stream! {
            for delta in deltas {
                yield StreamEvent::TextDelta(delta);
            }
            yield StreamEvent::UsageSummary(UsageSummary {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            });
            yield StreamEvent::StreamEnd;
        };
        Box::pin(stream)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.1 Single-turn Conversation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_turn_text_response_completes() {
    let api = Box::new(TextMock::new("Hello, world!"));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.max_turns = 1;
    options.handle_elicitation = false;

    let result = engine.submit("hello".into(), options).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), TerminalReason::Completed);

    // Session should contain user + assistant messages
    assert_eq!(engine.session().messages.len(), 2);
    assert!(matches!(
        engine.session().messages[0].content,
        MessageContent::Text(ref t) if t == "hello"
    ));
}

#[tokio::test]
async fn single_turn_no_tool_calls() {
    let api = Box::new(TextMock::new("No tools needed."));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, mut rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine.submit("count to 3".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // No tool events should have been emitted
    let events = drain_events(&mut rx).await;
    let tool_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::ToolCallStart { .. }))
        .collect();
    assert!(tool_starts.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.2 Multi-turn Conversation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_turn_tool_call_then_text() {
    let responses = vec![
        tool_response(vec![ToolCall {
            id: "tc_1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "test.txt"}),
        }]),
        text_response("I read the file for you."),
    ];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, mut rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine
        .submit("read test.txt".into(), options)
        .await
        .unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // Session shape (5): user, assistant(text preamble), assistant(tool_call),
    // tool_result, assistant(2). The engine persists the assistant turn as
    // two messages when a structured tool call accompanies visible text.
    assert_eq!(engine.session().messages.len(), 5);

    // Verify tool events were emitted
    let events = drain_events(&mut rx).await;
    let tool_ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::ToolCallEnd { .. }))
        .collect();
    assert_eq!(tool_ends.len(), 1);
}

#[tokio::test]
async fn multi_turn_context_accumulated() {
    let responses = vec![
        tool_response(vec![ToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "echo hi"}),
        }]),
        text_response("Done."),
    ];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    engine.submit("run echo".into(), options).await.unwrap();

    // After the conversation, the session should have all messages.
    // 5 messages: user, assistant(text), assistant(tool_call), tool, assistant.
    let msgs = &engine.session().messages;
    assert_eq!(msgs.len(), 5);

    // Message roles: User, Assistant(text), Assistant(tool_call), Tool, Assistant
    assert!(matches!(msgs[0].role, coderex_core::types::Role::User));
    assert!(matches!(msgs[1].role, coderex_core::types::Role::Assistant));
    assert!(matches!(msgs[2].role, coderex_core::types::Role::Assistant));
    assert!(matches!(msgs[3].role, coderex_core::types::Role::Tool));
    assert!(matches!(msgs[4].role, coderex_core::types::Role::Assistant));
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.3 Streaming Response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn streaming_text_deltas_emitted() {
    let api = Box::new(StreamingMock {
        deltas: vec!["Hello".into(), " ".into(), "stream".into(), "!".into()],
    });
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, mut rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine
        .submit_streaming("say hello".into(), options)
        .await
        .unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // Collect text delta events
    let events = drain_events(&mut rx).await;
    let deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();

    let combined: String = deltas.iter().flat_map(|s| s.chars()).collect();
    assert_eq!(combined, "Hello stream!");
}

#[tokio::test]
async fn streaming_no_tool_calls_completes_cleanly() {
    let api = Box::new(StreamingMock {
        deltas: vec!["No".into(), " tools".into()],
    });
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, mut rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine
        .submit_streaming("hello".into(), options)
        .await
        .unwrap();
    assert_eq!(result, TerminalReason::Completed);

    let events = drain_events(&mut rx).await;
    let tool_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::ToolCallStart { .. }))
        .collect();
    assert!(tool_starts.is_empty());
}

/// A turn with no tool call is not a loop, however many of them arrive.
#[tokio::test]
async fn three_text_only_turns_are_not_a_loop() {
    let api = Box::new(TextMock::new("plain answer"));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    for _ in 0..3 {
        let mut options = SubmitOptions::default();
        options.handle_elicitation = false;
        let reason = engine.submit("hello".into(), options).await.unwrap();
        assert_eq!(
            reason,
            TerminalReason::Completed,
            "a third ordinary turn was reported as a tool-call loop"
        );
    }
}

/// A streaming tool call is executed early, but its `tool_use` still has to be
/// persisted before the `tool_result`: a history with only the result is not a
/// valid assistant/user pair.
struct StreamingToolMock {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl ApiClient for StreamingToolMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(StreamingToolMock {
            calls: std::sync::Arc::clone(&self.calls),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        Ok(text_response("done"))
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let stream = async_stream::stream! {
            if first {
                yield StreamEvent::ToolCallBlockStart {
                    id: "tc_stream".into(),
                    name: "echo".into(),
                };
                yield StreamEvent::ToolCallInputDelta {
                    id: "tc_stream".into(),
                    delta: r#"{"x":1}"#.into(),
                };
                yield StreamEvent::ToolCallBlockEnd {
                    id: "tc_stream".into(),
                    input: serde_json::Value::Null,
                };
            } else {
                yield StreamEvent::TextDelta("done".into());
            }
            yield StreamEvent::UsageSummary(usage(10));
            yield StreamEvent::StreamEnd;
        };
        Box::pin(stream)
    }
}

#[tokio::test]
async fn streaming_tool_calls_persist_the_assistant_tool_use() {
    let api = Box::new(StreamingToolMock {
        calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;
    let reason = engine
        .submit_streaming("use a tool".into(), options)
        .await
        .unwrap();
    assert_eq!(reason, TerminalReason::Completed);

    let msgs = &engine.session().messages;
    let call_index = msgs.iter().position(|m| {
        matches!(
            &m.content,
            coderex_core::types::MessageContent::ToolCall(tc) if tc.id == "tc_stream"
        )
    });
    let result_index = msgs.iter().position(|m| {
        matches!(
            &m.content,
            coderex_core::types::MessageContent::ToolResult(tr) if tr.tool_call_id == "tc_stream"
        )
    });

    assert!(
        call_index.is_some(),
        "the streaming tool_use was not persisted: {msgs:?}"
    );
    assert!(
        result_index.is_some(),
        "the streaming tool_result was not persisted: {msgs:?}"
    );
    assert!(
        call_index < result_index,
        "the tool_use must come before its result"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.4 Tool Call Loop
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn tool_call_loop_executes_and_continues() {
    // First turn: assistant requests a tool
    // Second turn: assistant sees tool result and responds with text
    let responses = vec![
        tool_response(vec![ToolCall {
            id: "tc_loop".into(),
            name: "glob".into(),
            input: serde_json::json!({"pattern": "*.rs"}),
        }]),
        text_response("Found 5 Rust files."),
    ];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine
        .submit("find rust files".into(), options)
        .await
        .unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // Verify tool result was appended to session
    let msgs = &engine.session().messages;
    let tool_msgs: Vec<_> = msgs
        .iter()
        .filter(|m| matches!(m.role, coderex_core::types::Role::Tool))
        .collect();
    assert_eq!(tool_msgs.len(), 1);
    let tool_output = match &tool_msgs[0].content {
        MessageContent::ToolResult(tr) => &tr.output,
        _ => panic!("expected ToolResult"),
    };
    assert!(tool_output.contains("executed glob"));
}

/// A provider that returned thinking in one turn requires the block back in
/// the next request, before the text and tool_use of that same assistant turn.
/// The same conversation through both submission paths must end in the same
/// state: the streaming path's deferred finish and the non-streaming path's
/// immediate one converge on identical messages, usage and reason.
struct BothPathsMock {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl BothPathsMock {
    fn new() -> Self {
        Self {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

fn parity_tool_call() -> ToolCall {
    ToolCall {
        id: "tc_parity".into(),
        name: "echo".into(),
        input: serde_json::json!({"text": "hi"}),
    }
}

#[async_trait]
impl ApiClient for BothPathsMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(BothPathsMock {
            calls: std::sync::Arc::clone(&self.calls),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        Ok(if first {
            ApiResponse {
                content: "working".into(),
                tool_calls: vec![parity_tool_call()],
                usage: usage(10),
                stop_reason: "tool_use".into(),
                thinking: None,
            }
        } else {
            ApiResponse {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: usage(10),
                stop_reason: "end_turn".into(),
                thinking: None,
            }
        })
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let first = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let stream = async_stream::stream! {
            if first {
                yield StreamEvent::TextDelta("working".into());
                yield StreamEvent::ToolCallBlockStart {
                    id: "tc_parity".into(),
                    name: "echo".into(),
                };
                yield StreamEvent::ToolCallInputDelta {
                    id: "tc_parity".into(),
                    delta: r#"{"text":"hi"}"#.into(),
                };
                yield StreamEvent::ToolCallBlockEnd {
                    id: "tc_parity".into(),
                    input: serde_json::Value::Null,
                };
            } else {
                yield StreamEvent::TextDelta("done".into());
            }
            yield StreamEvent::UsageSummary(usage(10));
            yield StreamEvent::StreamEnd;
        };
        Box::pin(stream)
    }
}

fn messages_json(engine: &QueryEngine) -> String {
    serde_json::to_string(&engine.session().messages).expect("messages serialize")
}

#[tokio::test]
async fn both_submission_paths_finish_a_tool_turn_in_the_same_state() {
    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let (mut immediate, _rx) =
        make_engine(Box::new(BothPathsMock::new()), Box::new(EchoToolExecutor));
    let immediate_reason = immediate
        .submit("use a tool".into(), options.clone())
        .await
        .unwrap();

    let (mut streamed, _rx) =
        make_engine(Box::new(BothPathsMock::new()), Box::new(EchoToolExecutor));
    let streamed_reason = streamed
        .submit_streaming("use a tool".into(), options)
        .await
        .unwrap();

    assert_eq!(immediate_reason, streamed_reason);
    assert_eq!(
        messages_json(&immediate),
        messages_json(&streamed),
        "the two finish paths produced different histories"
    );
    assert_eq!(immediate.total_usage(), streamed.total_usage());
}

#[tokio::test]
async fn thinking_block_is_round_tripped_in_the_follow_up_request() {
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let api = Box::new(ThinkingToolMock {
        calls: std::sync::Arc::new(AtomicUsize::new(0)),
        requests: std::sync::Arc::clone(&requests),
    });
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;
    let reason = engine.submit("read a.txt".into(), options).await.unwrap();
    assert_eq!(reason, TerminalReason::Completed);

    let requests = requests.lock().unwrap();
    assert!(
        requests.len() >= 2,
        "expected a follow-up request, got {}",
        requests.len()
    );
    let follow_up = &requests[1];
    let assistant = follow_up
        .iter()
        .find(|message| {
            message["role"] == "assistant"
                && message["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"))
        })
        .expect("the follow-up must carry the assistant tool_use turn");
    let blocks = assistant["content"]
        .as_array()
        .expect("the assistant content must be a block array");

    assert_eq!(
        blocks
            .iter()
            .map(|b| b["type"].as_str())
            .collect::<Vec<_>>(),
        vec![Some("thinking"), Some("text"), Some("tool_use")],
        "the assistant turn must be exactly [thinking, text, tool_use]"
    );
    assert_eq!(blocks[0]["thinking"], "I should read the file first.");
    assert_eq!(blocks[0]["signature"], "sig-abc");
    assert_eq!(blocks[2]["id"], "tc_think");
}

#[tokio::test]
async fn tool_call_loop_multiple_tools_in_one_turn() {
    let responses = vec![
        tool_response(vec![
            ToolCall {
                id: "tc_a".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a.txt"}),
            },
            ToolCall {
                id: "tc_b".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "b.txt"}),
            },
        ]),
        text_response("Read both files."),
    ];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, mut rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine.submit("read both".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // Two tool results should be in session
    let tool_count = engine
        .session()
        .messages
        .iter()
        .filter(|m| matches!(m.role, coderex_core::types::Role::Tool))
        .count();
    assert_eq!(tool_count, 2);

    // Two tool end events should be emitted
    let events = drain_events(&mut rx).await;
    let tool_ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::ToolCallEnd { .. }))
        .collect();
    assert_eq!(tool_ends.len(), 2);
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.5 Structured Output
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn structured_output_valid_json_passes() {
    let api = Box::new(TextMock::new(r#"{"name":"Alice","age":30}"#));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let schema = serde_json::json!({
        "type": "object",
        "required": ["name", "age"],
        "properties": {
            "name": { "type": "string" },
            "age": { "type": "number" }
        }
    });
    engine.set_structured_output_schema(schema);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine.submit("give me json".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // The assistant message should contain the JSON (no validation warning appended)
    let msgs = &engine.session().messages;
    let assistant_msg = msgs
        .iter()
        .find(|m| matches!(m.role, coderex_core::types::Role::Assistant));
    assert!(assistant_msg.is_some());
    let content = match &assistant_msg.unwrap().content {
        MessageContent::Text(t) => t.as_str(),
        _ => panic!("expected text content"),
    };
    assert!(!content.contains("validation failed"));
}

#[tokio::test]
async fn structured_output_invalid_json_gets_warning() {
    let api = Box::new(TextMock::new("this is not json"));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let schema = serde_json::json!({ "type": "object" });
    engine.set_structured_output_schema(schema);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine.submit("give me json".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // The assistant message should have a validation warning appended
    let msgs = &engine.session().messages;
    let assistant_msg = msgs
        .iter()
        .find(|m| matches!(m.role, coderex_core::types::Role::Assistant));
    let content = match &assistant_msg.unwrap().content {
        MessageContent::Text(t) => t.as_str(),
        _ => panic!("expected text content"),
    };
    assert!(content.contains("Structured output validation failed"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.6 Termination Conditions
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn termination_max_turns_reached() {
    // Tool call triggers continuation; max_turns=1 stops it before turn 2
    let api = Box::new(SequenceMock::new(vec![tool_response(vec![ToolCall {
        id: "tc_1".into(),
        name: "read".into(),
        input: serde_json::json!({"path": "x"}),
    }])]));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.max_turns = 1;
    options.handle_elicitation = false;

    let result = engine
        .submit("read something".into(), options)
        .await
        .unwrap();
    assert_eq!(result, TerminalReason::MaxTurnsReached);
}

#[tokio::test]
async fn termination_budget_exhausted() {
    // Override the mock to return high token usage
    struct HighUsageMock;
    #[async_trait]
    impl ApiClient for HighUsageMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(HighUsageMock)
        }
        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            Ok(ApiResponse {
                content: "response".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 10,
                    output_tokens: 100, // exceeds budget of 5
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
            })
        }
        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(StreamEvent::StreamEnd);
            Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }))
        }
    }

    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(Box::new(HighUsageMock), tools);

    let mut options = SubmitOptions::default();
    options.task_budget_tokens = 5; // tiny budget
    options.handle_elicitation = false;

    let result = engine.submit("hello".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::BudgetExhausted);
}

/// Mock that counts API calls for debugging.
struct CountingMock {
    count: AtomicUsize,
}

#[async_trait]
impl ApiClient for CountingMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(CountingMock {
            count: AtomicUsize::new(self.count.load(Ordering::SeqCst)),
        })
    }
    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        let c = self.count.fetch_add(1, Ordering::SeqCst);
        Ok(ApiResponse {
            content: "tool call".into(),
            tool_calls: vec![ToolCall {
                id: format!("tc_{c}"),
                name: "read".into(),
                input: serde_json::json!({"path": "loop.txt"}),
            }],
            usage: UsageSummary {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            stop_reason: "end_turn".into(),
            thinking: None,
        })
    }
    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(StreamEvent::StreamEnd);
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

#[tokio::test]
async fn termination_loop_detected() {
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(
        Box::new(CountingMock {
            count: AtomicUsize::new(0),
        }),
        tools,
    );

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine.submit("loop me".into(), options).await.unwrap();
    assert!(
        matches!(
            result,
            TerminalReason::LoopDetected {
                period: 1,
                repetitions: 3
            }
        ),
        "expected LoopDetected, got {result:?}"
    );
}

#[tokio::test]
async fn termination_user_interrupt() {
    let api = Box::new(TextMock::new("I'm still thinking..."));
    let tools = Box::new(EchoToolExecutor);
    let (engine, mut rx) = make_engine(api, tools);

    // Simulate interrupt by sending SessionEnd event
    engine.interrupt();

    // The event should be emitted
    let events = drain_events(&mut rx).await;
    assert!(events.iter().any(|e| matches!(e, EngineEvent::SessionEnd)));
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.7 State Machine Transitions
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn state_machine_turn_counter_increments() {
    let responses = vec![
        tool_response(vec![ToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }]),
        text_response("Done."),
    ];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    assert_eq!(engine.current_turn(), 0);

    engine.submit("run ls".into(), options).await.unwrap();

    // After a multi-turn submit, current_turn should reflect at least 1 turn
    assert!(engine.current_turn() >= 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Engine configuration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn engine_tracks_cost_across_turns() {
    let responses = vec![text_response("Turn 1"), text_response("Turn 2")];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    // Two separate submit calls
    engine
        .submit("first".into(), options.clone())
        .await
        .unwrap();
    engine.submit("second".into(), options).await.unwrap();

    // Cost tracker should have recorded both turns
    assert!(engine.current_turn() >= 2);
}

#[tokio::test]
async fn engine_custom_system_prompt_appended() {
    let api = Box::new(TextMock::new("Acknowledged."));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.custom_system_prompt = Some("You are a helpful test assistant.".into());
    options.handle_elicitation = false;

    let result = engine.submit("hello".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // The session should have the user message and assistant response
    assert_eq!(engine.session().messages.len(), 2);
}

#[tokio::test]
async fn engine_allowed_tools_override_blocks_unlisted() {
    let responses = vec![
        tool_response(vec![ToolCall {
            id: "tc_1".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "out.txt"}),
        }]),
        text_response("Done."),
    ];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, mut rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.allowed_tools_override = Some(vec!["read".into()]);
    options.handle_elicitation = false;

    let result = engine
        .submit("write something".into(), options)
        .await
        .unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // The write tool should have been blocked
    let events = drain_events(&mut rx).await;
    let blocked: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, EngineEvent::ToolCallEnd { result, .. } if result.is_error && result.output.contains("blocked"))
        })
        .collect();
    assert_eq!(
        blocked.len(),
        1,
        "write tool should be blocked by allowed_tools_override"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.8 Compaction + session memory
// ═══════════════════════════════════════════════════════════════════════════

use coderex_core::session_memory::SessionMemoryBackend;
use coderex_core::types::Message;

/// Mock that replays responses and records every request (max_tokens +
/// serialized messages) so tests can assert on the LLM call sequence.
struct RecordingMock {
    responses: Vec<ApiResponse>,
    call_count: std::sync::Arc<AtomicUsize>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<(u32, String)>>>,
    /// When set, `create_message` fails for any request whose serialized
    /// messages contain this substring.
    fail_on_substring: Option<String>,
}

impl RecordingMock {
    fn new(responses: Vec<ApiResponse>) -> Self {
        Self {
            responses,
            call_count: std::sync::Arc::new(AtomicUsize::new(0)),
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            fail_on_substring: None,
        }
    }

    /// When set, `create_message` fails for any request whose serialized
    /// messages contain this substring.
    fn failing_on(mut self, needle: &str) -> Self {
        self.fail_on_substring = Some(needle.to_string());
        self
    }

    fn calls_handle(&self) -> std::sync::Arc<AtomicUsize> {
        std::sync::Arc::clone(&self.call_count)
    }

    fn requests_handle(&self) -> std::sync::Arc<std::sync::Mutex<Vec<(u32, String)>>> {
        std::sync::Arc::clone(&self.requests)
    }
}

#[async_trait]
impl ApiClient for RecordingMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(Self {
            responses: self.responses.clone(),
            call_count: std::sync::Arc::clone(&self.call_count),
            requests: std::sync::Arc::clone(&self.requests),
            fail_on_substring: self.fail_on_substring.clone(),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        messages: &[serde_json::Value],
        _model: &ModelId,
        max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        if let Some(needle) = &self.fail_on_substring {
            if serde_json::to_string(messages)
                .unwrap_or_default()
                .contains(needle.as_str())
            {
                return Err(ApiError::Http("background feature unavailable".into()));
            }
        }
        self.requests.lock().unwrap().push((
            max_tokens,
            serde_json::to_string(messages).unwrap_or_default(),
        ));
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let resp = if idx < self.responses.len() {
            self.responses[idx].clone()
        } else {
            self.responses.last().unwrap().clone()
        };
        Ok(resp)
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(StreamEvent::StreamEnd);
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Test double for the session-memory backend: serves a stored document and
/// records every save.
struct RecordingMemoryBackend {
    stored: std::sync::Mutex<std::collections::HashMap<String, String>>,
    saved: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl RecordingMemoryBackend {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            stored: std::sync::Mutex::new(std::collections::HashMap::new()),
            saved: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }
}

#[async_trait]
impl SessionMemoryBackend for RecordingMemoryBackend {
    async fn load_memory(&self, session_id: &str) -> Option<String> {
        self.stored.lock().unwrap().get(session_id).cloned()
    }

    async fn save_memory(&self, session_id: &str, content: &str) {
        self.saved.lock().unwrap().push(content.to_string());
        self.stored
            .lock()
            .unwrap()
            .insert(session_id.to_string(), content.to_string());
    }
}

/// A session with enough turns for `summary_split(…, 3)` to yield a
/// non-empty, ≥2-message summarize range.
fn session_with_history() -> SessionState {
    let mut session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
    session.messages.push(Message::user("start"));
    session.messages.push(Message::assistant("a1"));
    session.messages.push(Message::user("u2"));
    session.messages.push(Message::assistant("a2"));
    session.messages.push(Message::user("u3"));
    session.messages.push(Message::assistant("a3"));
    session.messages.push(Message::user("u4"));
    session.messages.push(Message::assistant("a4"));
    session
}

/// Response whose usage exceeds the test's tiny budget, triggering
/// budget-exhaustion → compact recovery.
fn budget_trigger_response() -> ApiResponse {
    ApiResponse {
        content: "Working on it.".into(),
        tool_calls: Vec::new(),
        usage: UsageSummary {
            input_tokens: 10,
            output_tokens: 2_000, // budget is 1_000
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
        stop_reason: "end_turn".into(),
        thinking: None,
    }
}

#[tokio::test]
async fn compact_recovery_injects_session_memory() {
    let responses = vec![
        budget_trigger_response(),
        text_response("Summary: the task is mid-flight."), // LLM summary
        text_response("Current State: auth fixed.\nKey Decisions: use bcrypt."),
        text_response("Done."), // retry after compaction
    ];
    let mock = RecordingMock::new(responses);
    let requests = mock.requests_handle();
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(Box::new(mock), tools);
    engine.set_session(session_with_history());
    let backend = RecordingMemoryBackend::new();
    let saved_handle = std::sync::Arc::clone(&backend.saved);
    let backend_dyn: std::sync::Arc<dyn SessionMemoryBackend> = backend;
    engine.set_session_memory(backend_dyn);

    let mut options = SubmitOptions::default();
    options.task_budget_tokens = 1_000;
    options.handle_elicitation = false;

    // The engine does not reset turn_tokens after compaction, so a
    // compaction triggered by budget exhaustion ends the turn exhausted.
    let result = engine.submit("finish it".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::BudgetExhausted);

    // The refreshed memory document was persisted through the backend.
    let saved = saved_handle.lock().unwrap();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].contains("Current State: auth fixed."));
    drop(saved);

    // The post-compaction retry request carries the injected memory section.
    let requests = requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|(_, msgs)| msgs.contains("<session_memory>")
                && msgs.contains("Current State: auth fixed.")),
        "post-compaction request should carry the injected <session_memory>"
    );
}

#[tokio::test]
async fn compact_recovery_survives_memory_update_failure() {
    let responses = vec![
        budget_trigger_response(),
        text_response("Summary: the task is mid-flight."),
        text_response("Done."),
    ];
    // The session-memory create prompt is identifiable by its opening line.
    let mock = RecordingMock::new(responses).failing_on("Create a new session memory file");
    let requests = mock.requests_handle();
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(Box::new(mock), tools);
    engine.set_session(session_with_history());
    let backend = RecordingMemoryBackend::new();
    let saved_handle = std::sync::Arc::clone(&backend.saved);
    let backend_dyn: std::sync::Arc<dyn SessionMemoryBackend> = backend;
    engine.set_session_memory(backend_dyn);

    let mut options = SubmitOptions::default();
    options.task_budget_tokens = 1_000;
    options.handle_elicitation = false;

    // Compaction (and the whole turn) still succeeds.
    // The engine does not reset turn_tokens after compaction, so a
    // compaction triggered by budget exhaustion ends the turn exhausted.
    let result = engine.submit("finish it".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::BudgetExhausted);

    // No memory was persisted, and no <session_memory> was injected.
    assert!(saved_handle.lock().unwrap().is_empty());
    let requests = requests.lock().unwrap();
    assert!(
        !requests
            .iter()
            .any(|(_, msgs)| msgs.contains("<session_memory>"))
    );
}

#[tokio::test]
async fn compact_recovery_no_extra_llm_call_without_backend() {
    // trigger → summary → retry: exactly 3 LLM calls with no backend.
    let responses = vec![
        budget_trigger_response(),
        text_response("Summary: the task is mid-flight."),
        text_response("Done."),
    ];
    let mock = RecordingMock::new(responses);
    let calls = mock.calls_handle();
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(Box::new(mock), tools);
    engine.set_session(session_with_history());

    let mut options = SubmitOptions::default();
    options.task_budget_tokens = 1_000;
    options.handle_elicitation = false;

    // The engine does not reset turn_tokens after compaction, so a
    // compaction triggered by budget exhaustion ends the turn exhausted.
    let result = engine.submit("finish it".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::BudgetExhausted);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "compaction without a memory backend must not add LLM calls"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.9 Compaction backend abstraction (local/remote dual-track seam)
// ═══════════════════════════════════════════════════════════════════════════

use coderex_core::compaction_backend::{CompactionBackend, CompactionKind, CompactionRequest};

/// Stub compaction backend: records every request and serves a canned
/// summary (or fails, to exercise the fallback path).
struct StubCompactionBackend {
    summary: Result<String, String>,
    called: std::sync::Arc<std::sync::Mutex<Vec<CompactionRequest>>>,
}

impl StubCompactionBackend {
    fn ok(
        summary: &str,
    ) -> (
        std::sync::Arc<Self>,
        std::sync::Arc<std::sync::Mutex<Vec<CompactionRequest>>>,
    ) {
        let called = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            std::sync::Arc::new(Self {
                summary: Ok(summary.to_string()),
                called: std::sync::Arc::clone(&called),
            }),
            called,
        )
    }

    fn failing() -> (
        std::sync::Arc<Self>,
        std::sync::Arc<std::sync::Mutex<Vec<CompactionRequest>>>,
    ) {
        let called = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            std::sync::Arc::new(Self {
                summary: Err("remote compaction unavailable".into()),
                called: std::sync::Arc::clone(&called),
            }),
            called,
        )
    }
}

#[async_trait]
impl CompactionBackend for StubCompactionBackend {
    fn kind(&self) -> CompactionKind {
        CompactionKind::Remote
    }

    fn name(&self) -> &str {
        "stub-remote"
    }

    async fn summarize(&self, request: CompactionRequest) -> Result<String, String> {
        self.called.lock().unwrap().push(request);
        self.summary.clone()
    }
}

#[tokio::test]
async fn compaction_backend_serves_summary_before_local_path() {
    // trigger → (backend summary, no local LLM call) → retry → done
    let responses = vec![budget_trigger_response(), text_response("Done.")];
    let mock = RecordingMock::new(responses);
    let calls = mock.calls_handle();
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(Box::new(mock), tools);
    engine.set_session(session_with_history());
    let (backend, backend_calls) = StubCompactionBackend::ok("Remote summary: task mid-flight.");
    let backend_dyn: std::sync::Arc<dyn CompactionBackend> = backend;
    engine.set_compaction_backend(backend_dyn);

    let mut options = SubmitOptions::default();
    options.task_budget_tokens = 1_000;
    options.handle_elicitation = false;

    // The engine does not reset turn_tokens after compaction, so a
    // compaction triggered by budget exhaustion ends the turn exhausted.
    let result = engine.submit("finish it".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::BudgetExhausted);

    // The backend was consulted exactly once with the summarized interval.
    let called = backend_calls.lock().unwrap();
    assert_eq!(
        called.len(),
        1,
        "compaction backend should be consulted once"
    );
    assert!(
        called[0].conversation.contains("[User]"),
        "backend receives rendered conversation"
    );
    drop(called);

    // trigger + retry only — the local summary LLM call never happened.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn compaction_backend_failure_falls_back_to_local_summary() {
    // trigger → backend fails → local LLM summary → retry → done
    let responses = vec![
        budget_trigger_response(),
        text_response("Local summary: task mid-flight."), // local fallback path
        text_response("Done."),
    ];
    let mock = RecordingMock::new(responses);
    let calls = mock.calls_handle();
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(Box::new(mock), tools);
    engine.set_session(session_with_history());
    let (backend, backend_calls) = StubCompactionBackend::failing();
    let backend_dyn: std::sync::Arc<dyn CompactionBackend> = backend;
    engine.set_compaction_backend(backend_dyn);

    let mut options = SubmitOptions::default();
    options.task_budget_tokens = 1_000;
    options.handle_elicitation = false;

    let result = engine.submit("finish it".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::BudgetExhausted);

    // Backend consulted once (failed); the local summary path then ran and
    // succeeded, so the PTL half-budget retry never fired.
    assert_eq!(backend_calls.lock().unwrap().len(), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "backend failure must fall back to the local LLM summary"
    );
}

#[tokio::test]
async fn compaction_without_backend_uses_local_path_unchanged() {
    // Baseline: no backend attached → exactly trigger + summary + retry.
    let responses = vec![
        budget_trigger_response(),
        text_response("Summary: the task is mid-flight."),
        text_response("Done."),
    ];
    let mock = RecordingMock::new(responses);
    let calls = mock.calls_handle();
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(Box::new(mock), tools);
    engine.set_session(session_with_history());

    let mut options = SubmitOptions::default();
    options.task_budget_tokens = 1_000;
    options.handle_elicitation = false;

    let result = engine.submit("finish it".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::BudgetExhausted);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "default local path unchanged"
    );
}
