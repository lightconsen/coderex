//! Integration tests for hook firing from `QueryEngine`.
//!
//! The hooks crate tests the registry and the CLI tests one end-to-end block;
//! these drive a full turn with a recording [`HookExecutor`] and a mock
//! provider, so the engine's firing sites and the way their results change the
//! request are covered without a subprocess.

use async_trait::async_trait;
use coderex_core::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use coderex_core::engine::{QueryEngine, SubmitOptions, TerminalReason};
use coderex_core::hook_executor::{HookExecutor, HookFireResult};
use coderex_core::permission_engine::PermissionEngine;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::state::SessionState;
use coderex_core::tool_executor::{ToolDispatchOutcome, ToolExecutor, ToolInfo};
use coderex_core::types::{
    ContextWindowOverrides, ContextWindowResolver, EngineEvent, ModelId, ToolCall, ToolResult,
    UsageSummary,
};
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

// ─── Recording hook executor ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Fired {
    event: String,
    input: String,
    context: HashMap<String, String>,
}

/// Records every event and answers with a per-event result.
#[derive(Default)]
struct RecordingHooks {
    fired: Mutex<Vec<Fired>>,
    answers: Mutex<HashMap<String, HookFireResult>>,
}

impl RecordingHooks {
    fn with(event: &str, result: HookFireResult) -> Self {
        let hooks = Self::default();
        hooks
            .answers
            .lock()
            .unwrap()
            .insert(event.to_string(), result);
        hooks
    }

    fn events(&self) -> Vec<String> {
        self.fired
            .lock()
            .unwrap()
            .iter()
            .map(|fired| fired.event.clone())
            .collect()
    }

    fn count(&self, event: &str) -> usize {
        self.fired
            .lock()
            .unwrap()
            .iter()
            .filter(|fired| fired.event == event)
            .count()
    }

    fn input_of(&self, event: &str) -> Option<String> {
        self.fired
            .lock()
            .unwrap()
            .iter()
            .find(|fired| fired.event == event)
            .map(|fired| fired.input.clone())
    }

    fn context_of(&self, event: &str) -> Option<HashMap<String, String>> {
        self.fired
            .lock()
            .unwrap()
            .iter()
            .find(|fired| fired.event == event)
            .map(|fired| fired.context.clone())
    }
}

#[async_trait]
impl HookExecutor for RecordingHooks {
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult {
        self.fired.lock().unwrap().push(Fired {
            event: event_name.to_string(),
            input: input.to_string(),
            context: context.clone(),
        });
        self.answers
            .lock()
            .unwrap()
            .get(event_name)
            .cloned()
            .unwrap_or_default()
    }
}

// ─── Mock provider ─────────────────────────────────────────────────────────

fn usage() -> UsageSummary {
    UsageSummary {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    }
}

fn text_response(content: &str) -> ApiResponse {
    ApiResponse {
        content: content.into(),
        tool_calls: Vec::new(),
        usage: usage(),
        stop_reason: "end_turn".into(),
        thinking: None,
    }
}

/// Returns one tool call, then a text answer, recording every request.
struct ToolThenTextMock {
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
    tool: &'static str,
}

impl ToolThenTextMock {
    fn new(tool: &'static str) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            tool,
        }
    }
}

#[async_trait]
impl ApiClient for ToolThenTextMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(Self {
            calls: Arc::clone(&self.calls),
            requests: Arc::clone(&self.requests),
            tool: self.tool,
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
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(ApiResponse {
                content: "I'll read it.".into(),
                tool_calls: vec![ToolCall {
                    id: "tc_1".into(),
                    name: self.tool.into(),
                    input: serde_json::json!({"file_path": "a.txt"}),
                }],
                usage: usage(),
                stop_reason: "tool_use".into(),
                thinking: None,
            })
        } else {
            Ok(text_response("done"))
        }
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let response = self
            .create_message(system, messages, model, max_tokens, tools)
            .await;
        let (tx, rx) = mpsc::unbounded_channel();
        match response {
            Ok(response) => {
                if !response.content.is_empty() {
                    let _ = tx.send(StreamEvent::TextDelta(response.content.clone()));
                }
                for call in &response.tool_calls {
                    let _ = tx.send(StreamEvent::ToolCallBlockStart {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    });
                    let _ = tx.send(StreamEvent::ToolCallInputDelta {
                        id: call.id.clone(),
                        delta: call.input.to_string(),
                    });
                    let _ = tx.send(StreamEvent::ToolCallBlockEnd {
                        id: call.id.clone(),
                        input: call.input.clone(),
                    });
                }
                let _ = tx.send(StreamEvent::StreamEnd);
            }
            Err(_) => {
                let _ = tx.send(StreamEvent::StreamEnd);
            }
        }
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Always answers with text, recording every request.
struct TextMock {
    requests: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
}

impl TextMock {
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ApiClient for TextMock {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(Self {
            requests: Arc::clone(&self.requests),
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
        Ok(text_response("ok"))
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(StreamEvent::TextDelta("ok".into()));
        let _ = tx.send(StreamEvent::StreamEnd);
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

// ─── Tool executor ─────────────────────────────────────────────────────────

struct CountingTools {
    calls: Arc<AtomicUsize>,
    is_error: bool,
}

impl CountingTools {
    fn ok() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            is_error: false,
        }
    }

    fn failing() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            is_error: true,
        }
    }
}

#[async_trait]
impl ToolExecutor for CountingTools {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        _input: &serde_json::Value,
    ) -> ToolDispatchOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolDispatchOutcome {
            result: ToolResult {
                tool_call_id: tool_call_id.to_string(),
                output: format!("{tool_name} output"),
                is_error: self.is_error,
                images: Vec::new(),
            },
        }
    }
}

// ─── Harness ───────────────────────────────────────────────────────────────

fn make_engine(
    api: Box<dyn ApiClient>,
    tools: Box<dyn ToolExecutor + Send + Sync>,
    hooks: Arc<RecordingHooks>,
) -> (QueryEngine, mpsc::UnboundedReceiver<EngineEvent>) {
    make_engine_with_mode(api, tools, hooks, PermissionMode::BypassPermissions)
}

fn make_engine_with_mode(
    api: Box<dyn ApiClient>,
    tools: Box<dyn ToolExecutor + Send + Sync>,
    hooks: Arc<RecordingHooks>,
    mode: PermissionMode,
) -> (QueryEngine, mpsc::UnboundedReceiver<EngineEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let checker = PermissionChecker::new(mode);
    let perm_engine = PermissionEngine::new(checker);
    let session = SessionState::new(std::path::PathBuf::from("/tmp/hook-test"));
    let mut engine = QueryEngine::new(api, ModelId::sonnet(), session, perm_engine, tx);
    engine.set_tool_executor(tools);
    engine.set_hooks(hooks);
    (engine, rx)
}

fn last_user_text(messages: &[serde_json::Value]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str().map(str::to_string))
        .unwrap_or_default()
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn user_prompt_submit_updated_input_reaches_the_provider() {
    let mock = TextMock::new();
    let requests = Arc::clone(&mock.requests);
    let hooks = Arc::new(RecordingHooks::with(
        "UserPromptSubmit",
        HookFireResult {
            updated_input: Some("normalised prompt".into()),
            ..HookFireResult::default()
        },
    ));
    let (mut engine, _rx) = make_engine(Box::new(mock), Box::new(CountingTools::ok()), hooks);

    engine
        .submit("raw prompt".into(), SubmitOptions::default())
        .await
        .expect("turn completes");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        last_user_text(&requests[0]),
        "normalised prompt",
        "the provider sees the hook's replacement: {:?}",
        requests[0]
    );
}

#[tokio::test]
async fn user_prompt_submit_stop_closes_the_turn_without_a_request() {
    let mock = TextMock::new();
    let requests = Arc::clone(&mock.requests);
    let hooks = Arc::new(RecordingHooks::with(
        "UserPromptSubmit",
        HookFireResult {
            stop: true,
            ..HookFireResult::default()
        },
    ));
    let (mut engine, _rx) = make_engine(Box::new(mock), Box::new(CountingTools::ok()), hooks);

    let reason = engine
        .submit("do not send".into(), SubmitOptions::default())
        .await
        .expect("the stopped turn still closes");

    assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
    assert!(requests.lock().unwrap().is_empty(), "no provider request");
}

#[tokio::test]
async fn pre_tool_use_stop_keeps_the_tool_from_running() {
    let api = ToolThenTextMock::new("read");
    let tools = CountingTools::ok();
    let calls = Arc::clone(&tools.calls);
    let hooks = Arc::new(RecordingHooks::with(
        "PreToolUse",
        HookFireResult {
            stop: true,
            ..HookFireResult::default()
        },
    ));
    let (mut engine, _rx) = make_engine(Box::new(api), Box::new(tools), Arc::clone(&hooks));

    engine
        .submit("read a.txt".into(), SubmitOptions::default())
        .await
        .expect("turn completes");

    assert_eq!(calls.load(Ordering::SeqCst), 0, "the tool never ran");
    assert_eq!(hooks.count("PreToolUse"), 1);
    assert_eq!(hooks.input_of("PreToolUse").as_deref(), Some("read"));
}

#[tokio::test]
async fn post_tool_use_rewrites_what_the_model_sees() {
    let api = ToolThenTextMock::new("read");
    let requests = Arc::clone(&api.requests);
    let hooks = Arc::new(RecordingHooks::with(
        "PostToolUse",
        HookFireResult {
            updated_tool_output: Some("redacted".into()),
            ..HookFireResult::default()
        },
    ));
    let (mut engine, _rx) = make_engine(Box::new(api), Box::new(CountingTools::ok()), hooks);

    engine
        .submit("read a.txt".into(), SubmitOptions::default())
        .await
        .expect("turn completes");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = serde_json::to_string(&requests[1]).unwrap();
    assert!(second.contains("redacted"), "{second}");
    assert!(!second.contains("read output"), "{second}");
}

#[tokio::test]
async fn post_tool_use_failure_fires_when_permission_denies_the_tool() {
    let api = ToolThenTextMock::new("write");
    let hooks = Arc::new(RecordingHooks::default());
    let (mut engine, _rx) = make_engine_with_mode(
        Box::new(api),
        Box::new(CountingTools::ok()),
        Arc::clone(&hooks),
        PermissionMode::Plan,
    );

    engine
        .submit("write a.txt".into(), SubmitOptions::default())
        .await
        .expect("turn completes");

    assert_eq!(hooks.count("PermissionDenied"), 1, "{:?}", hooks.events());
    assert_eq!(hooks.count("PostToolUseFailure"), 1, "{:?}", hooks.events());
}

#[tokio::test]
async fn a_tool_that_reports_an_error_still_fires_post_tool_use() {
    // The engine distinguishes "the tool ran and answered with an error" from
    // "permission denied the tool": the former is a normal PostToolUse, and
    // its output is still available to a rewriting hook.
    let api = ToolThenTextMock::new("read");
    let hooks = Arc::new(RecordingHooks::default());
    let (mut engine, _rx) = make_engine(
        Box::new(api),
        Box::new(CountingTools::failing()),
        Arc::clone(&hooks),
    );

    engine
        .submit("read a.txt".into(), SubmitOptions::default())
        .await
        .expect("turn completes");

    assert_eq!(hooks.count("PostToolUse"), 1, "{:?}", hooks.events());
    assert_eq!(hooks.count("PostToolUseFailure"), 0, "{:?}", hooks.events());
}

#[tokio::test]
async fn session_start_stop_and_turn_end_fire_once_per_turn() {
    let api = TextMock::new();
    let hooks = Arc::new(RecordingHooks::default());
    let (mut engine, _rx) =
        make_engine(Box::new(api), Box::new(CountingTools::ok()), hooks.clone());

    engine
        .submit("hello".into(), SubmitOptions::default())
        .await
        .expect("turn completes");

    assert_eq!(hooks.count("SessionStart"), 1, "{:?}", hooks.events());
    let context = hooks.context_of("SessionStart").expect("fired");
    assert!(context.contains_key("session_id"), "{context:?}");
    assert_eq!(
        context.get("cwd").map(String::as_str),
        Some("/tmp/hook-test")
    );
    assert_eq!(hooks.count("UserPromptSubmit"), 1, "{:?}", hooks.events());
    assert_eq!(hooks.count("Stop"), 1, "{:?}", hooks.events());
    assert_eq!(hooks.count("TurnEnd"), 1, "{:?}", hooks.events());
}

#[tokio::test]
async fn ndjson_resume_replay_still_fires_user_prompt_submit() {
    // A replayed conversation is still a prompt for the hook: it must be able
    // to inspect or rewrite what the model is about to see.
    let api = TextMock::new();
    let hooks = Arc::new(RecordingHooks::default());
    let (mut engine, _rx) =
        make_engine(Box::new(api), Box::new(CountingTools::ok()), hooks.clone());

    let options = SubmitOptions {
        replay_user_messages: Some(vec![coderex_core::types::Message::user("replayed")]),
        ..SubmitOptions::default()
    };
    engine
        .submit("new prompt".into(), options)
        .await
        .expect("turn");

    assert_eq!(hooks.count("UserPromptSubmit"), 1, "{:?}", hooks.events());
}

#[tokio::test]
async fn pre_compact_and_post_compact_fire_around_a_threshold_compaction() {
    // A tiny context window makes the first request over the compaction line,
    // so the turn compacts before sending anything.
    let api = TextMock::new();
    let hooks = Arc::new(RecordingHooks::default());
    let (mut engine, _rx) =
        make_engine(Box::new(api), Box::new(CountingTools::ok()), hooks.clone());
    engine.set_context_window_resolver(ContextWindowResolver::new(
        ContextWindowOverrides {
            default: Some(200),
            ..ContextWindowOverrides::default()
        },
        ContextWindowOverrides::default(),
        ContextWindowOverrides::default(),
        ContextWindowOverrides::default(),
    ));

    engine
        .submit(
            "a prompt that will not fit the tiny window".into(),
            SubmitOptions::default(),
        )
        .await
        .expect("the turn finishes");

    assert!(hooks.count("PreCompact") >= 1, "{:?}", hooks.events());
    assert!(hooks.count("PostCompact") >= 1, "{:?}", hooks.events());
}
