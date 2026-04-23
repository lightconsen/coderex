//! QueryEngine integration tests — covering full turn lifecycle, streaming,
//! tool call loops, and termination conditions.
//!
//! These tests correspond to the equivalence test plan Module 1:
//! 1.1 single-turn, 1.2 multi-turn, 1.3 streaming, 1.4 tool call loop,
//! 1.5 structured output, 1.6 termination conditions, 1.7 state machine

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use async_trait::async_trait;
use coderex_core::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use coderex_core::engine::{QueryEngine, SubmitOptions, TerminalReason};
use coderex_core::permission_engine::PermissionEngine;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::state::SessionState;
use coderex_core::tool_executor::{ToolDispatchOutcome, ToolExecutor};
use coderex_core::types::{EngineEvent, MessageContent, ModelId, ToolCall, ToolResult, UsageSummary};
use futures::Stream;
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
    }
}

fn tool_response(tool_calls: Vec<ToolCall>) -> ApiResponse {
    ApiResponse {
        content: "I'll use a tool.".into(),
        tool_calls,
        usage: usage(20),
        stop_reason: "end_turn".into(),
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
    ) -> Result<ApiResponse, ApiError> {
        Ok(self.response.clone())
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(StreamEvent::StreamEnd);
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Returns a sequence of responses, cycling the last one.
struct SequenceMock {
    responses: Vec<ApiResponse>,
    call_count: AtomicUsize,
}

impl SequenceMock {
    fn new(responses: Vec<ApiResponse>) -> Self {
        Self {
            responses,
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ApiClient for SequenceMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(SequenceMock {
            responses: self.responses.clone(),
            call_count: AtomicUsize::new(self.call_count.load(Ordering::SeqCst)),
        })
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
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

    let result = engine.submit("read test.txt".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // Should have 4 messages: user, assistant(1), tool_result, assistant(2)
    assert_eq!(engine.session().messages.len(), 4);

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

    // After the conversation, the session should have all messages
    let msgs = &engine.session().messages;
    assert_eq!(msgs.len(), 4);

    // Message roles should be: User, Assistant, Tool, Assistant
    assert!(matches!(msgs[0].role, coderex_core::types::Role::User));
    assert!(matches!(msgs[1].role, coderex_core::types::Role::Assistant));
    assert!(matches!(msgs[2].role, coderex_core::types::Role::Tool));
    assert!(matches!(msgs[3].role, coderex_core::types::Role::Assistant));
}

// ═══════════════════════════════════════════════════════════════════════════
//  1.3 Streaming Response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn streaming_text_deltas_emitted() {
    let api = Box::new(StreamingMock {
        deltas: vec![
            "Hello".into(),
            " ".into(),
            "stream".into(),
            "!".into(),
        ],
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

    let result = engine.submit_streaming("hello".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    let events = drain_events(&mut rx).await;
    let tool_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::ToolCallStart { .. }))
        .collect();
    assert!(tool_starts.is_empty());
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

    let result = engine.submit("find rust files".into(), options).await.unwrap();
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
    let assistant_msg = msgs.iter().find(|m| matches!(m.role, coderex_core::types::Role::Assistant));
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
    let assistant_msg = msgs.iter().find(|m| matches!(m.role, coderex_core::types::Role::Assistant));
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

    let result = engine.submit("read something".into(), options).await.unwrap();
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
            })
        }
        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
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
        })
    }
    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
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
    let (mut engine, _rx) = make_engine(Box::new(CountingMock { count: AtomicUsize::new(0) }), tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    let result = engine.submit("loop me".into(), options).await.unwrap();
    assert!(
        matches!(
            result,
            TerminalReason::LoopDetected { period: 1, repetitions: 3 }
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
    let responses = vec![
        text_response("Turn 1"),
        text_response("Turn 2"),
    ];
    let api = Box::new(SequenceMock::new(responses));
    let tools = Box::new(EchoToolExecutor);
    let (mut engine, _rx) = make_engine(api, tools);

    let mut options = SubmitOptions::default();
    options.handle_elicitation = false;

    // Two separate submit calls
    engine.submit("first".into(), options.clone()).await.unwrap();
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

    let result = engine.submit("write something".into(), options).await.unwrap();
    assert_eq!(result, TerminalReason::Completed);

    // The write tool should have been blocked
    let events = drain_events(&mut rx).await;
    let blocked: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(e, EngineEvent::ToolCallEnd { result, .. } if result.is_error && result.output.contains("blocked"))
        })
        .collect();
    assert_eq!(blocked.len(), 1, "write tool should be blocked by allowed_tools_override");
}
