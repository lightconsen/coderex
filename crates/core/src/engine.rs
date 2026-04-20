//! TurnState state machine — lifecycle states, terminal reasons, and transitions.
//!
//! The turn state machine drives a single conversation turn through:
//! 1. Context building (system prompts, memory, MCP, tools)
//! 2. API request (streaming or non-streaming)
//! 3. Tool execution (with permission checks)
//! 4. Continuation evaluation (should we keep going?)
//! 5. Compact/recovery if needed
//! 6. Finalize and emit events

use tokio::sync::mpsc;
use tokio::time::Instant;
use std::collections::HashMap;
use std::sync::Arc;

use crate::types::{EngineEvent, Message, ModelId, ToolCall, ToolResult, UsageSummary};
use crate::budget::TokenBudget;
use crate::cost::CostTracker;
use crate::permission_engine::{PermissionEngine, PermissionResponse};
use crate::permissions::{PermissionMode, PermissionResult};
use crate::api_client::{ApiClient, ApiResponse, StreamEvent};
use crate::state::SessionState;
use crate::prompt::{AssembledContext, CompactConfig, PromptCompiler, PromptContext};
use crate::compact::{self, CompactResult, CompactStrategy};
use crate::stop_hook::{self, StopHookContext, StopHookDecision};
use crate::hook_executor::{HookExecutor, HookFireResult, NoOpHookExecutor};
use crate::tool_executor::{NoOpToolExecutor, ToolExecutor};
use crate::tool_scheduler;
use crate::elicitation::{ElicitationHandler, ElicitationResult, ReplElicitationHandler};
use crate::loop_detector::{LoopDetection, LoopDetector};
use std::path::PathBuf;

// ─── Structured Output Validation ───────────────────────────────────────────

/// Result of validating output against a JSON schema.
#[derive(Debug, Clone)]
pub enum ValidationResult {
    /// Output is valid according to the schema.
    Valid(serde_json::Value),
    /// Output is invalid — contains the error and retry count.
    Invalid {
        /// Validation error message.
        error: String,
        /// How many times this output has been retried.
        retries: u32,
    },
}

/// Validates structured output against a JSON schema.
///
/// Supports: type checking, required fields, enum constraints,
/// and nested object validation.
#[derive(Debug, Clone)]
pub struct StructuredOutputValidator {
    schema: serde_json::Value,
    max_retries: u32,
}

impl StructuredOutputValidator {
    /// Create a new validator with the given schema.
    #[must_use]
    pub const fn new(schema: serde_json::Value) -> Self {
        Self {
            schema,
            max_retries: 3,
        }
    }

    /// Set the maximum number of retries for invalid output.
    #[must_use]
    pub const fn with_max_retries(mut self, max: u32) -> Self {
        self.max_retries = max;
        self
    }

    /// Validate output against the schema.
    #[must_use]
    pub fn validate(&self, output: &str, retries: u32) -> ValidationResult {
        if retries >= self.max_retries {
            return ValidationResult::Invalid {
                error: format!("max retries ({}) exceeded", self.max_retries),
                retries,
            };
        }

        let value: serde_json::Value = match serde_json::from_str(output) {
            Ok(v) => v,
            Err(e) => {
                return ValidationResult::Invalid {
                    error: format!("invalid JSON: {e}"),
                    retries,
                };
            }
        };

        match Self::validate_value(&value, &self.schema) {
            Ok(()) => ValidationResult::Valid(value),
            Err(error) => ValidationResult::Invalid { error, retries },
        }
    }

    /// Recursively validate a JSON value against a schema.
    fn validate_value(value: &serde_json::Value, schema: &serde_json::Value) -> Result<(), String> {
        let Some(schema_obj) = schema.as_object() else {
            return Ok(());
        };

        // Type check
        if let Some(expected_type) = schema_obj.get("type") {
            if let Some(type_str) = expected_type.as_str() {
                Self::check_type(value, type_str)?;
            }
        }

        // Enum check
        if let Some(enum_values) = schema_obj.get("enum") {
            if let Some(arr) = enum_values.as_array() {
                if !arr.contains(value) {
                    return Err(format!("value not in enum: {value}"));
                }
            }
        }

        // Object required fields
        if let Some(properties) = schema_obj.get("properties") {
            if let Some(props) = properties.as_object() {
                if let Some(obj) = value.as_object() {
                    for (key, prop_schema) in props {
                        if let Some(val) = obj.get(key) {
                            Self::validate_value(val, prop_schema)?;
                        }
                    }
                }
            }
        }

        // Required fields check
        if let Some(required) = schema_obj.get("required") {
            if let Some(arr) = required.as_array() {
                if let Some(obj) = value.as_object() {
                    for field in arr {
                        if let Some(field_str) = field.as_str() {
                            if !obj.contains_key(field_str) {
                                return Err(format!("missing required field: {field_str}"));
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Check if a value matches the expected JSON type.
    fn check_type(value: &serde_json::Value, expected: &str) -> Result<(), String> {
        let actual = match value {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::Object(_) => "object",
        };
        if actual != expected {
            return Err(format!("expected type '{expected}', got '{actual}'"));
        }
        Ok(())
    }

    /// Generate a retry instruction message for the model.
    #[must_use]
    pub fn retry_instruction(&self, error: &str) -> String {
        format!(
            "Your previous response did not match the required JSON schema. Error: {error}. Please respond with ONLY valid JSON matching the schema."
        )
    }
}

// ─── Streaming Tool Call Assembler ──────────────────────────────────────────

/// Accumulates streaming tool call deltas into complete tool calls.
///
/// During streaming, tool calls arrive incrementally:
/// 1. `ToolCallBlockStart` gives id + name
/// 2. `ToolCallInputDelta` gives JSON fragments
/// 3. `ToolCallBlockEnd` gives complete JSON (or assembler builds it)
#[derive(Debug, Clone)]
pub struct StreamingToolCallAssembler {
    /// Accumulating tool call buffers: id -> (name, input_json_builder).
    pending: HashMap<String, (String, String)>,
    /// Completed tool calls.
    completed: Vec<ToolCall>,
    /// Text content accumulated alongside tool calls.
    text_content: String,
    /// Whether any tool calls have been seen.
    has_tool_calls: bool,
}

impl StreamingToolCallAssembler {
    /// Create a new assembler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            completed: Vec::new(),
            text_content: String::new(),
            has_tool_calls: false,
        }
    }

    /// Process a stream event and return any completed tool calls.
    ///
    /// Returns `Vec<ToolCall>` when a tool call is fully assembled.
    /// Also returns `true` if the stream has ended (no more events expected).
    pub fn process_event(&mut self, event: StreamEvent) -> Vec<ToolCall> {
        match event {
            StreamEvent::TextDelta(text) => {
                self.text_content.push_str(&text);
                Vec::new()
            }
            StreamEvent::ToolCallBlockStart { id, name } => {
                self.has_tool_calls = true;
                self.pending.insert(id, (name, String::new()));
                Vec::new()
            }
            StreamEvent::ToolCallInputDelta { id, delta } => {
                if let Some((_, builder)) = self.pending.get_mut(&id) {
                    builder.push_str(&delta);
                }
                Vec::new()
            }
            StreamEvent::ToolCallBlockEnd { id, input } => {
                let completed = if let Some((name, builder)) = self.pending.remove(&id) {
                    let input = if input.is_null() {
                        match serde_json::from_str(&builder) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "tool call {id} ({name}) has malformed JSON input: {e}\naccumulated: {builder}"
                                );
                                return Vec::new();
                            }
                        }
                    } else {
                        input
                    };
                    let call = ToolCall { id, name, input };
                    self.completed.push(call.clone());
                    vec![call]
                } else {
                    Vec::new()
                };
                completed
            }
            StreamEvent::StreamEnd => {
                // Finalize any pending tool calls from accumulated JSON
                let mut calls = Vec::new();
                for (id, (name, builder)) in self.pending.drain() {
                    match serde_json::from_str(&builder) {
                        Ok(input) => {
                            let call = ToolCall { id, name, input };
                            self.completed.push(call.clone());
                            calls.push(call);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "tool call {id} ({name}) has malformed JSON input at stream end: {e}\naccumulated: {builder}"
                            );
                        }
                    }
                }
                calls
            }
            StreamEvent::UsageSummary(_) | StreamEvent::Error(_) => Vec::new(),
        }
    }

    /// Get the accumulated text content.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text_content
    }

    /// Check if any tool calls have been seen.
    #[must_use]
    pub const fn has_tool_calls(&self) -> bool {
        self.has_tool_calls
    }

    /// Get completed tool calls (consumes them).
    pub fn take_completed(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.completed)
    }
}

impl Default for StreamingToolCallAssembler {
    fn default() -> Self {
        Self::new()
    }
}

// ─── TurnState ──────────────────────────────────────────────────────────────

/// Lifecycle states for a single conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// Initial state — preparing the turn.
    Initialize,
    /// Building the prompt context (system prompts, memory, MCP, etc.).
    BuildContext,
    /// Running pre-submit hooks.
    RunPreSubmitHooks,
    /// Sending the request to the API.
    StartRequest,
    /// Streaming assistant response tokens.
    StreamAssistant,
    /// Parsing and executing tool calls.
    ExecuteTools,
    /// Waiting for user permission to execute a tool.
    AwaitPermission,
    /// Waiting for user elicitation (clarification question).
    AwaitElicitation,
    /// Applying tool results back into the message history.
    ApplyToolResults,
    /// Evaluating whether to continue (more turns/continuations).
    EvaluateContinuation,
    /// Running compact/recovery if context is too large.
    CompactRecovery,
    /// Running stop hooks before finalizing.
    StopHooks,
    /// Finalizing the turn — emitting end events.
    Finalize,
    /// The turn has terminated (successfully or with error).
    Terminated,
}

impl TurnState {
    /// Returns true if this state waits for user input.
    #[must_use]
    pub const fn is_user_waiting(&self) -> bool {
        matches!(self, Self::AwaitPermission | Self::AwaitElicitation)
    }

    /// Returns true if this state is a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminated)
    }
}

// ─── TerminalReason ─────────────────────────────────────────────────────────

/// Reason why a turn terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalReason {
    /// The turn completed normally (no more tool calls, response finished).
    Completed,
    /// The user interrupted the turn (Ctrl+C or explicit cancel).
    UserInterrupted,
    /// The token budget was exhausted.
    BudgetExhausted,
    /// The maximum number of turns was reached.
    MaxTurnsReached,
    /// An error occurred during execution.
    Error(String),
    /// A stop hook decided the conversation should end.
    StopHook,
    /// The model's response was stopped due to token limit.
    TokenLimitStop,
    /// A repetitive loop of tool calls was detected.
    LoopDetected {
        /// Period of the repeating pattern (1 = exact repeat, 2 = A-B, etc.).
        period: usize,
        /// How many times the pattern repeated.
        repetitions: usize,
    },
    /// The API is unavailable after retry exhaustion.
    ApiUnavailable {
        /// Human-readable explanation.
        message: String,
    },
}

impl std::fmt::Display for TerminalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => write!(f, "completed"),
            Self::UserInterrupted => write!(f, "user interrupted"),
            Self::BudgetExhausted => write!(f, "budget exhausted"),
            Self::MaxTurnsReached => write!(f, "max turns reached"),
            Self::Error(msg) => write!(f, "error: {msg}"),
            Self::StopHook => write!(f, "stop hook"),
            Self::TokenLimitStop => write!(f, "token limit stop"),
            Self::LoopDetected { period, repetitions } => {
                write!(f, "loop detected (period={period}, repetitions={repetitions})")
            }
            Self::ApiUnavailable { message } => {
                write!(f, "api unavailable: {message}")
            }
        }
    }
}

// ─── PendingItem ────────────────────────────────────────────────────────────

/// An assembled tool call ready for execution.
#[derive(Debug, Clone)]
pub struct PendingToolCall {
    /// Unique tool call ID.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// JSON input arguments.
    pub input: serde_json::Value,
}

impl From<&ToolCall> for PendingToolCall {
    fn from(tc: &ToolCall) -> Self {
        Self {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: tc.input.clone(),
        }
    }
}

/// A resolved permission response.
#[derive(Debug, Clone)]
pub struct ResolvedPermission {
    /// The tool call ID this permission is for.
    pub tool_call_id: String,
    /// Whether the user approved.
    pub response: PermissionResponse,
}

/// A pending elevation request awaiting user response.
#[derive(Debug, Clone)]
pub struct PendingElevation {
    /// The tool call ID that needs elevation.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// JSON input arguments.
    pub input: serde_json::Value,
    /// Request ID for the event.
    pub request_id: String,
}

/// A resolved elicitation response.
#[derive(Debug, Clone)]
pub struct ResolvedElicitation {
    /// The elicitation request ID.
    pub request_id: String,
    /// User's response (if approved).
    pub response: Option<String>,
}

// ─── SubmitOptions ──────────────────────────────────────────────────────────

/// Options for submitting a turn to the QueryEngine.
#[derive(Debug, Clone)]
pub struct SubmitOptions {
    /// Maximum number of turns to run (0 = unlimited).
    pub max_turns: u32,
    /// Maximum budget in USD for this submission (0 = use session default).
    pub max_budget_usd: f64,
    /// Token budget override (0 = use default thresholds).
    pub task_budget_tokens: u64,
    /// Optional JSON schema for structured output.
    pub json_schema: Option<serde_json::Value>,
    /// Custom system prompt to append.
    pub custom_system_prompt: Option<String>,
    /// System prompt to append for this turn only.
    pub append_system_prompt: Option<String>,
    /// User messages to replay (for session resume).
    pub replay_user_messages: Option<Vec<Message>>,
    /// Include partial messages in output.
    pub include_partial_messages: bool,
    /// Whether to handle elicitation requests.
    pub handle_elicitation: bool,
    /// Recover orphaned permission requests from disk.
    pub orphaned_permission: bool,
    /// Restrict tool execution to the given tool names for this submission.
    pub allowed_tools_override: Option<Vec<String>>,
}

impl Default for SubmitOptions {
    fn default() -> Self {
        Self {
            max_turns: 0,
            max_budget_usd: 0.0,
            task_budget_tokens: 0,
            json_schema: None,
            custom_system_prompt: None,
            append_system_prompt: None,
            replay_user_messages: None,
            include_partial_messages: false,
            handle_elicitation: true,
            orphaned_permission: false,
            allowed_tools_override: None,
        }
    }
}

// ─── TurnRuntime ────────────────────────────────────────────────────────────

/// Per-turn runtime data — state, messages, pending items, budget.
#[allow(missing_debug_implementations)]
pub struct TurnRuntime {
    /// Current state machine state.
    pub state: TurnState,
    /// Messages for this turn.
    pub messages: Vec<Message>,
    /// Pending tool calls awaiting execution.
    pub pending_tool_calls: Vec<PendingToolCall>,
    /// Pending permission request.
    pub pending_permission: Option<ResolvedPermission>,
    /// Pending elevation request awaiting user response.
    pub pending_elevation: Option<PendingElevation>,
    /// Pending elicitation request.
    pub pending_elicitation: Option<ResolvedElicitation>,
    /// Token budget tracker.
    pub budget: TokenBudget,
    /// Compact/recovery retry counter.
    pub recovery_count: u32,
    /// Whether the stop hook is active.
    pub stop_hook_active: bool,
    /// Terminal reason (set when state transitions to Terminated).
    pub terminal_reason: Option<TerminalReason>,
}

impl TurnRuntime {
    /// Create a new turn runtime.
    #[must_use]
    pub const fn new(budget: TokenBudget) -> Self {
        Self {
            state: TurnState::Initialize,
            messages: Vec::new(),
            pending_tool_calls: Vec::new(),
            pending_permission: None,
            pending_elevation: None,
            pending_elicitation: None,
            budget,
            recovery_count: 0,
            stop_hook_active: false,
            terminal_reason: None,
        }
    }

    /// Reset for a new turn.
    pub fn reset(&mut self) {
        self.state = TurnState::Initialize;
        self.pending_tool_calls.clear();
        self.pending_permission = None;
        self.pending_elevation = None;
        self.pending_elicitation = None;
        self.budget.reset_turn();
    }

    /// Transition to a new state.
    pub const fn transition_to(&mut self, new_state: TurnState) {
        self.state = new_state;
    }

    /// Terminate with a reason.
    pub fn terminate(&mut self, reason: TerminalReason) {
        self.state = TurnState::Terminated;
        self.terminal_reason = Some(reason);
    }
}

// ─── TurnProgress ───────────────────────────────────────────────────────────

/// Progress event emitted during turn execution.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub enum TurnProgress {
    /// State transition occurred.
    StateChanged {
        from: TurnState,
        to: TurnState,
    },
    /// A text delta was received from the model.
    TextDelta(String),
    /// A tool call was started.
    ToolCallStarted { id: String, name: String },
    /// A tool call completed.
    ToolCallCompleted { id: String, result: ToolResult },
    /// A permission request was emitted.
    PermissionRequested {
        id: String,
        tool: String,
        args: serde_json::Value,
    },
    /// An elicitation request was emitted.
    ElicitationRequested {
        id: String,
        message: String,
    },
    /// A compact boundary was inserted.
    CompactBoundary,
    /// The turn completed.
    TurnCompleted {
        turn_number: u32,
        usage: UsageSummary,
        reason: TerminalReason,
    },
    /// An error occurred.
    Error(String),
}

// ─── QueryEngine ────────────────────────────────────────────────────────────

/// QueryEngine — manages conversation turns with the AI model.
///
/// The engine handles:
/// - Building context from session state
/// - Sending requests to the API
/// - Streaming responses and parsing tool calls
/// - Permission checks before tool execution
/// - Tool execution via the registry
/// - Token budget and cost tracking
/// - Compact/recovery logic
/// - Elicitation requests
#[allow(missing_debug_implementations)]
pub struct QueryEngine {
    /// API client for making requests.
    api_client: Box<dyn ApiClient>,
    /// Model to use for API requests.
    model: ModelId,
    /// Session state for the current conversation.
    session: SessionState,
    /// Cost tracker for billing.
    cost_tracker: CostTracker,
    /// Permission engine for tool approval.
    permission_engine: PermissionEngine,
    /// Event sender for streaming events to UI/SDK.
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Current turn number (1-indexed).
    current_turn: u32,
    /// Denied tool calls from previous turns, injected into next prompt.
    denied_tools: Vec<DeniedToolCall>,
    /// Schema validator for structured output.
    structured_output: Option<StructuredOutputValidator>,
    /// Prompt compiler for system prompt assembly.
    prompt_compiler: PromptCompiler,
    /// Compact configuration.
    compact_config: CompactConfig,
    /// Hook executor for firing lifecycle hooks.
    hooks: Box<dyn HookExecutor + Send + Sync>,
    /// Tool executor for dispatching tool calls.
    tools: Box<dyn ToolExecutor + Send + Sync>,
    /// Handler for elicitation requests.
    elicitation_handler: Box<dyn ElicitationHandler + Send + Sync>,
    /// Optional transcript base path. When set, compact updates the
    /// transcript file on disk by removing old messages and writing a
    /// compact boundary marker.
    transcript_path: Option<PathBuf>,
    /// Usage summary from the most recent model request.
    last_usage: Option<UsageSummary>,
    /// Loop detector for detecting repetitive tool call patterns.
    loop_detector: Option<LoopDetector>,
    /// Last successful response content and usage (for graceful degradation).
    last_successful_response: Option<(String, UsageSummary)>,
    /// Optional team context for multi-agent collaboration.
    team_context: Option<Arc<std::sync::Mutex<crate::team_context::TeamContext>>>,
    /// Optional coordinator state for multi-phase workflows.
    coordinator: Option<crate::coordinator::CoordinatorState>,
}

/// A denied tool call that should be summarized in the next turn.
#[derive(Debug, Clone)]
pub struct DeniedToolCall {
    /// Tool name.
    pub name: String,
    /// Reason for denial.
    pub reason: String,
}

impl QueryEngine {
    /// Create a new QueryEngine.
    #[must_use]
    pub fn new(
        api_client: Box<dyn ApiClient>,
        model: ModelId,
        session: SessionState,
        permission_engine: PermissionEngine,
        event_tx: mpsc::UnboundedSender<EngineEvent>,
    ) -> Self {
        let config = CompactConfig::default();
        Self {
            api_client,
            model,
            session,
            cost_tracker: CostTracker::new(),
            permission_engine,
            event_tx,
            current_turn: 0,
            denied_tools: Vec::new(),
            structured_output: None,
            prompt_compiler: PromptCompiler::new(config),
            compact_config: config,
            hooks: Box::new(NoOpHookExecutor),
            tools: Box::new(NoOpToolExecutor),
            elicitation_handler: Box::new(ReplElicitationHandler),
            transcript_path: None,
            last_usage: None,
            loop_detector: Some(LoopDetector::new()),
            last_successful_response: None,
            team_context: None,
            coordinator: None,
        }
    }

    /// Set the elicitation handler.
    pub fn set_elicitation_handler(
        &mut self,
        handler: Box<dyn ElicitationHandler + Send + Sync>,
    ) {
        self.elicitation_handler = handler;
    }

    /// Set the hook executor for lifecycle hooks.
    pub fn set_hooks(&mut self, hooks: Box<dyn HookExecutor + Send + Sync>) {
        self.hooks = hooks;
    }

    /// Set the tool executor for tool call dispatch.
    pub fn set_tool_executor(&mut self, tools: Box<dyn ToolExecutor + Send + Sync>) {
        self.tools = tools;
    }

    /// Set the loop detector for detecting repetitive tool call patterns.
    ///
    /// Pass `None` to disable loop detection.
    pub fn set_loop_detector(&mut self, detector: Option<LoopDetector>) {
        self.loop_detector = detector;
    }

    /// Set the transcript base path for persisting compact boundaries.
    ///
    /// When set, compact recovery will update the transcript file on disk
    /// by removing old messages and writing a compact boundary marker.
    pub fn set_transcript_path(&mut self, path: PathBuf) {
        self.transcript_path = Some(path);
    }

    /// Set the team context for multi-agent collaboration.
    pub fn set_team_context(
        &mut self,
        context: Arc<std::sync::Mutex<crate::team_context::TeamContext>>,
    ) {
        self.team_context = Some(context);
    }

    /// Get a formatted cost summary from the cost tracker.
    #[must_use]
    pub fn cost_summary(&self) -> String {
        self.cost_tracker.format_summary()
    }

    /// Set the coordinator state for multi-phase workflows.
    pub fn set_coordinator(&mut self, coordinator: crate::coordinator::CoordinatorState) {
        self.coordinator = Some(coordinator);
    }

    /// Recover orphaned permission requests from disk and emit them as
    /// `EngineEvent::PermissionRequest` so the UI can surface them.
    pub fn recover_orphaned_permissions(&mut self) {
        let Ok(requests) = self.permission_engine.recover_orphaned() else {
            return;
        };
        for req in requests {
            let _ = self.event_tx.send(EngineEvent::PermissionRequest {
                id: req.id,
                tool: req.tool_name,
                args: req.arguments.parse().unwrap_or_else(|_| serde_json::Value::Null),
            });
        }
    }

    /// Record tool calls from a turn and check for loops.
    ///
    /// Returns `Some(TerminalReason::LoopDetected)` if a loop is detected.
    fn record_and_check_loop(&mut self, tool_calls: &[PendingToolCall]) -> Option<TerminalReason> {
        let Some(ref mut detector) = self.loop_detector else {
            return None;
        };

        let calls: Vec<ToolCall> = tool_calls
            .iter()
            .map(|c| ToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                input: c.input.clone(),
            })
            .collect();

        if calls.is_empty() {
            detector.record_empty_turn();
        } else {
            detector.record_turn(&calls);
        }

        match detector.check() {
            LoopDetection::Ok => None,
            LoopDetection::LoopDetected {
                period,
                repetitions,
                description,
            } => {
                tracing::warn!("Loop detected: {description}");
                Some(TerminalReason::LoopDetected {
                    period,
                    repetitions,
                })
            }
        }
    }

    /// Compact session messages by keeping only the first user message
    /// and the most recent `keep_recent_turns` turns.
    ///
    /// Returns the compact result including how many tokens were removed
    /// and whether a boundary was inserted.
    pub fn compact_session_messages(&mut self, keep_recent_turns: usize) -> CompactResult {
        let result = compact::reactive_compact(&self.session.messages, keep_recent_turns);
        self.session.messages.clone_from(&result.messages);
        result
    }

    /// Handle an elicitation request using the configured elicitation handler.
    ///
    /// Transitions the turn to `AwaitElicitation`, sends the elicitation via
    /// the handler, and stores the response.
    async fn handle_elicitation(
        &mut self,
        turn: &mut TurnRuntime,
        request_id: String,
        message: String,
    ) -> ElicitationResult {
        turn.transition_to(TurnState::AwaitElicitation);

        // Emit elicitation event
        let _ = self.event_tx.send(EngineEvent::ElicitationRequest {
            id: request_id.clone(),
            message: message.clone(),
        });

        // Use the elicitation handler
        let question = crate::elicitation::ElicitationQuestion {
            question: message,
            header: "Query".into(),
            options: vec![
                crate::elicitation::ElicitationOption {
                    label: "Continue".into(),
                    description: "Proceed with the current approach".into(),
                },
                crate::elicitation::ElicitationOption {
                    label: "Clarify".into(),
                    description: "Provide additional clarification".into(),
                },
            ],
            multi_select: false,
        };

        let result = self.elicitation_handler.request(vec![question]).await;

        // Store the response
        match &result {
            ElicitationResult::Responded(resp) => {
                turn.pending_elicitation = Some(ResolvedElicitation {
                    request_id,
                    response: resp.other_text.clone().or_else(|| resp.selected.first().cloned()),
                });
            }
            ElicitationResult::Cancelled | ElicitationResult::TimedOut => {
                turn.pending_elicitation = Some(ResolvedElicitation {
                    request_id,
                    response: None,
                });
            }
        }

        result
    }

    /// Fire a hook event and return the result.
    async fn fire_hook(&self, event_name: &str, input: &str, context: &std::collections::HashMap<String, String>) -> HookFireResult {
        self.hooks.fire_hook(event_name, input, context).await
    }

    /// Fire the TurnEnd hook.
    async fn fire_turn_end_hook(&self, turn_number: u32, stop_reason: &str) -> HookFireResult {
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("turn_number".to_string(), turn_number.to_string());
        ctx.insert("stop_reason".to_string(), stop_reason.to_string());
        self.hooks.fire_hook("TurnEnd", &format!("turn_{turn_number}"), &ctx).await
    }

    /// Set the JSON schema for structured output validation.
    pub fn set_structured_output_schema(&mut self, schema: serde_json::Value) {
        self.structured_output = Some(StructuredOutputValidator::new(schema));
    }

    /// Get the denied tool calls summary.
    #[must_use]
    pub fn denied_tools(&self) -> &[DeniedToolCall] {
        &self.denied_tools
    }

    /// Clear denied tool calls (called after they've been injected).
    pub fn clear_denied_tools(&mut self) {
        self.denied_tools.clear();
    }

    /// Get the usage summary recorded for the most recent request.
    #[must_use]
    pub const fn last_usage(&self) -> Option<UsageSummary> {
        self.last_usage
    }

    /// Get the total token usage accumulated across all completed turns.
    #[must_use]
    pub fn total_usage(&self) -> UsageSummary {
        let mut total = UsageSummary {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        for turn in self.cost_tracker.get_turn_history() {
            total.input_tokens += turn.usage.input_tokens;
            total.output_tokens += turn.usage.output_tokens;
            total.cache_read_tokens += turn.usage.cache_read_tokens;
            total.cache_creation_tokens += turn.usage.cache_creation_tokens;
        }
        total
    }

    /// Submit a user message and run the full turn loop.
    ///
    /// This is the main entry point for conversation turns. It will:
    /// 1. Add the user message to the session
    /// 2. Build context (system prompts, memory, etc.)
    /// 3. Send to API and stream response
    /// 4. Execute any tool calls (with permission checks)
    /// 5. Evaluate continuation
    /// 6. Repeat until done or max_turns reached
    pub async fn submit(
        &mut self,
        prompt: String,
        options: SubmitOptions,
    ) -> Result<TerminalReason, String> {
        let budget = if options.task_budget_tokens > 0 {
            let mut budget = TokenBudget::with_defaults();
            budget.thresholds.max_tokens_per_session = options.task_budget_tokens;
            budget
        } else {
            TokenBudget::with_defaults()
        };

        let mut turn = TurnRuntime::new(budget);
        let mut turn_number = self.current_turn + 1;

        // Add user message
        self.session.messages.push(Message::user(prompt.clone()));

        // Main turn loop
        loop {
            if options.max_turns > 0 && turn_number > options.max_turns {
                turn.terminate(TerminalReason::MaxTurnsReached);
                break;
            }

            self.cost_tracker.start_turn();
            turn.reset();

            // Execute the turn
            match self.execute_turn(&mut turn, &options, turn_number).await {
                Ok(reason) => {
                    self.current_turn = turn_number;

                    // Advance coordinator phase if present
                    if let Some(ref mut coordinator) = self.coordinator {
                        let summary = format!("Turn {turn_number}: {reason}");
                        let _ = coordinator.advance(&summary);
                    }

                    if matches!(
                        reason,
                        TerminalReason::Completed
                            | TerminalReason::TokenLimitStop
                            | TerminalReason::LoopDetected { .. }
                            | TerminalReason::ApiUnavailable { .. }
                    ) {
                        // Check if we should continue (more turns needed for tool results)
                        break;
                    }
                    turn_number += 1;
                }
                Err(e) if e.starts_with("The AI service is temporarily unavailable") => {
                    let reason = TerminalReason::ApiUnavailable { message: e };
                    turn.terminate(reason.clone());
                    self.current_turn = turn_number;
                    return Ok(reason);
                }
                Err(e) => {
                    turn.terminate(TerminalReason::Error(e.clone()));
                    self.current_turn = turn_number;
                    return Err(e);
                }
            }

            if turn.state.is_terminal() {
                if let Some(reason) = turn.terminal_reason.take() {
                    return Ok(reason);
                }
            }
        }

        Ok(turn.terminal_reason.unwrap_or(TerminalReason::Completed))
    }

    /// Submit a user message and run the full turn loop using streaming API.
    ///
    /// This is the streaming variant — tokens are emitted as they arrive,
    /// and tool calls are assembled incrementally during the stream.
    pub async fn submit_streaming(
        &mut self,
        prompt: String,
        options: SubmitOptions,
    ) -> Result<TerminalReason, String> {
        let budget = if options.task_budget_tokens > 0 {
            let mut budget = TokenBudget::with_defaults();
            budget.thresholds.max_tokens_per_session = options.task_budget_tokens;
            budget
        } else {
            TokenBudget::with_defaults()
        };

        let mut turn = TurnRuntime::new(budget);
        let mut turn_number = self.current_turn + 1;

        // Add user message
        self.session.messages.push(Message::user(prompt.clone()));

        // Main turn loop
        loop {
            if options.max_turns > 0 && turn_number > options.max_turns {
                turn.terminate(TerminalReason::MaxTurnsReached);
                break;
            }

            self.cost_tracker.start_turn();
            turn.reset();

            // Execute the turn using streaming
            match self.execute_turn_streaming(&mut turn, &options, turn_number).await {
                Ok(reason) => {
                    self.current_turn = turn_number;

                    // Advance coordinator phase if present
                    if let Some(ref mut coordinator) = self.coordinator {
                        let summary = format!("Turn {turn_number}: {reason}");
                        let _ = coordinator.advance(&summary);
                    }

                    if matches!(
                        reason,
                        TerminalReason::Completed
                            | TerminalReason::TokenLimitStop
                            | TerminalReason::LoopDetected { .. }
                            | TerminalReason::ApiUnavailable { .. }
                    ) {
                        break;
                    }
                    turn_number += 1;
                }
                Err(e) if e.starts_with("The AI service is temporarily unavailable") => {
                    let reason = TerminalReason::ApiUnavailable { message: e };
                    turn.terminate(reason.clone());
                    self.current_turn = turn_number;
                    return Ok(reason);
                }
                Err(e) => {
                    turn.terminate(TerminalReason::Error(e.clone()));
                    self.current_turn = turn_number;
                    return Err(e);
                }
            }

            if turn.state.is_terminal() {
                if let Some(reason) = turn.terminal_reason.take() {
                    return Ok(reason);
                }
            }
        }

        Ok(turn.terminal_reason.unwrap_or(TerminalReason::Completed))
    }

    /// Execute a single turn through the state machine.
    async fn execute_turn(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Result<TerminalReason, String> {
        let _start = Instant::now();

        // Phase 1: Initialize → BuildContext
        turn.transition_to(TurnState::BuildContext);
        self.build_context(turn, options);

        // Phase 2: BuildContext → RunPreSubmitHooks
        turn.transition_to(TurnState::RunPreSubmitHooks);

        // Fire UserPromptSubmit hook
        let user_prompt = options
            .custom_system_prompt
            .as_deref()
            .unwrap_or_else(|| options.append_system_prompt.as_deref().unwrap_or(""));
        let ctx = std::collections::HashMap::new();
        let hook_result = self.fire_hook("UserPromptSubmit", user_prompt, &ctx).await;
        if hook_result.stop {
            turn.terminate(TerminalReason::Completed);
            return Ok(TerminalReason::Completed);
        }

        // Phase 3: RunPreSubmitHooks → StartRequest
        turn.transition_to(TurnState::StartRequest);

        // Phase 4: StartRequest → StreamAssistant
        turn.transition_to(TurnState::StreamAssistant);
        let api_response = self.send_request(turn, options).await?;

        // Record budget and cost
        turn.budget.record_tokens(&api_response.usage);
        self.cost_tracker.record_usage(&api_response.usage, &self.model);
        self.last_usage = Some(api_response.usage);

        // Parse the response
        let content = self.validate_structured_output(api_response.content.clone());
        let assistant_msg = Message::assistant(content.clone());
        turn.messages.push(assistant_msg);

        // Elicitation heuristic: if the assistant asks a short question, prompt the user
        let mut api_response = api_response;
        if options.handle_elicitation && content.trim().ends_with('?') && content.len() < 200 {
            let result = self.handle_elicitation(turn, format!("elic_{turn_number}"), content).await;
            if let ElicitationResult::Responded(resp) = result {
                if let Some(user_text) = resp.other_text.or_else(|| resp.selected.first().cloned()) {
                    let user_msg = Message::user(user_text);
                    turn.messages.push(user_msg.clone());
                    self.session.messages.push(user_msg);
                    // Re-send request with clarified context
                    api_response = self.send_request(turn, options).await?;
                    turn.budget.record_tokens(&api_response.usage);
                    self.cost_tracker.record_usage(&api_response.usage, &self.model);
                    self.last_usage = Some(api_response.usage);
                    let new_content = self.validate_structured_output(api_response.content.clone());
                    turn.messages.push(Message::assistant(new_content));
                }
            }
        }

        // Use structured tool calls from the API response (blocking path).
        let tool_calls = api_response.tool_calls;

        if tool_calls.is_empty() {
            // No tool calls — turn is complete
            turn.transition_to(TurnState::EvaluateContinuation);
            if !turn.budget.should_continue() {
                // Try compact recovery before giving up
                if turn.recovery_count < self.compact_config.max_recovery_retries {
                    turn.transition_to(TurnState::CompactRecovery);
                    if let Err(e) = self.run_compact_recovery(turn, options, turn_number).await {
                        turn.terminate(TerminalReason::Error(e.clone()));
                        return Ok(TerminalReason::Error(e));
                    }
                    // Retry the request with compacted context
                    turn.transition_to(TurnState::StartRequest);
                    let api_response = self.send_request(turn, options).await?;
                    turn.budget.record_tokens(&api_response.usage);
                    self.cost_tracker.record_usage(&api_response.usage, &self.model);
                    self.last_usage = Some(api_response.usage);
                    let content = self.validate_structured_output(api_response.content.clone());
                    let assistant_msg = Message::assistant(content);
                    turn.messages.push(assistant_msg);

                    // After recovery retry, check if we still need to compact
                    if !turn.budget.should_continue() {
                        turn.terminate(TerminalReason::BudgetExhausted);
                        return Ok(TerminalReason::BudgetExhausted);
                    }

                    turn.transition_to(TurnState::Finalize);
                    self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);
                    let _ = self.fire_turn_end_hook(turn_number, &api_response.stop_reason).await;

                    if api_response.stop_reason == "max_tokens" {
                        if let Some(reason) = self.record_and_check_loop(&[]) {
                            return Ok(reason);
                        }
                        return Ok(TerminalReason::TokenLimitStop);
                    }
                    if let Some(reason) = self.record_and_check_loop(&[]) {
                        return Ok(reason);
                    }
                    return Ok(TerminalReason::Completed);
                }

                turn.terminate(TerminalReason::BudgetExhausted);
                return Ok(TerminalReason::BudgetExhausted);
            }

            turn.transition_to(TurnState::Finalize);
            self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);
            let _ = self.fire_turn_end_hook(turn_number, &api_response.stop_reason).await;

            if api_response.stop_reason == "max_tokens" {
                if let Some(reason) = self.record_and_check_loop(&[]) {
                    return Ok(reason);
                }
                return Ok(TerminalReason::TokenLimitStop);
            }
            if let Some(reason) = self.record_and_check_loop(&[]) {
                return Ok(reason);
            }
            return Ok(TerminalReason::Completed);
        }

        // Phase 5: Execute tools
        turn.pending_tool_calls = tool_calls
            .iter()
            .map(PendingToolCall::from)
            .collect();
        let pending_for_loop = turn.pending_tool_calls.clone();

        turn.transition_to(TurnState::ExecuteTools);
        let tool_results = self.execute_tool_calls(turn, options).await?;

        // Phase 6: Apply tool results
        turn.transition_to(TurnState::ApplyToolResults);
        for result in &tool_results {
            let msg = Message::tool(result.clone());
            self.session.messages.push(msg);
        }

        // Phase 7: Evaluate continuation
        turn.transition_to(TurnState::EvaluateContinuation);
        if !turn.budget.should_continue() {
            // Try compact recovery before giving up
            if turn.recovery_count < self.compact_config.max_recovery_retries {
                turn.transition_to(TurnState::CompactRecovery);
                if let Err(e) = self.run_compact_recovery(turn, options, turn_number).await {
                    turn.terminate(TerminalReason::Error(e.clone()));
                    self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);
                    return Ok(TerminalReason::Error(e));
                }
                // After compact, tool results are still pending — the caller
                // will start the next turn with the compacted context
            } else {
                turn.terminate(TerminalReason::BudgetExhausted);
                self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);
                return Ok(TerminalReason::BudgetExhausted);
            }
        }

        turn.transition_to(TurnState::Finalize);
        self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);
        let _ = self.fire_turn_end_hook(turn_number, &api_response.stop_reason).await;

        // If we have tool results, the model needs another turn
        // Return Completed here — the caller will start the next turn
        if let Some(reason) = self.record_and_check_loop(&pending_for_loop) {
            return Ok(reason);
        }
        Ok(TerminalReason::Completed)
    }

    /// Execute a single turn using the streaming API.
    ///
    /// This variant uses `create_message_streaming()` for real-time token
    /// streaming and the `StreamingToolCallAssembler` to incrementally
    /// parse tool calls from the response.
    ///
    /// Key difference from `execute_turn`:
    /// - Text is streamed and emitted as it arrives
    /// - Tool calls are assembled incrementally during the stream
    /// - Early tool execution is possible (tool calls can be executed
    ///   as they complete, before the full response is received)
    async fn execute_turn_streaming(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Result<TerminalReason, String> {
        let _start = Instant::now();

        // Phase 1: Initialize → BuildContext
        turn.transition_to(TurnState::BuildContext);
        self.build_context(turn, options);

        // Phase 2: BuildContext → RunPreSubmitHooks
        turn.transition_to(TurnState::RunPreSubmitHooks);

        // Fire UserPromptSubmit hook
        let user_prompt = options
            .custom_system_prompt
            .as_deref()
            .unwrap_or_else(|| options.append_system_prompt.as_deref().unwrap_or(""));
        let ctx = std::collections::HashMap::new();
        let hook_result = self.fire_hook("UserPromptSubmit", user_prompt, &ctx).await;
        if hook_result.stop {
            turn.terminate(TerminalReason::Completed);
            return Ok(TerminalReason::Completed);
        }

        // Phase 3: RunPreSubmitHooks → StartRequest → StreamAssistant
        turn.transition_to(TurnState::StartRequest);
        turn.transition_to(TurnState::StreamAssistant);

        // Streaming request
        let (mut content, mut tool_calls, mut early_results, mut usage) =
            self.send_request_streaming(turn, options).await?;

        // Record budget and cost
        turn.budget.record_tokens(&usage);
        self.cost_tracker.record_usage(&usage, &self.model);
        self.last_usage = Some(usage);

        // Save assistant message
        let assistant_msg = Message::assistant(content.clone());
        turn.messages.push(assistant_msg);

        // Elicitation heuristic: if the assistant asks a short question, prompt the user
        if options.handle_elicitation && content.trim().ends_with('?') && content.len() < 200 {
            let result = self.handle_elicitation(turn, format!("elic_{turn_number}"), content).await;
            if let ElicitationResult::Responded(resp) = result {
                if let Some(user_text) = resp.other_text.or_else(|| resp.selected.first().cloned()) {
                    let user_msg = Message::user(user_text);
                    turn.messages.push(user_msg.clone());
                    self.session.messages.push(user_msg);
                    // Re-send request with clarified context
                    let (new_content, new_tool_calls, new_early_results, new_usage) =
                        self.send_request_streaming(turn, options).await?;
                    turn.budget.record_tokens(&new_usage);
                    self.cost_tracker.record_usage(&new_usage, &self.model);
                    self.last_usage = Some(new_usage);
                    content = new_content;
                    tool_calls = new_tool_calls;
                    early_results = new_early_results;
                    usage = new_usage;
                    turn.messages.push(Message::assistant(content.clone()));
                }
            }
        }

        // Apply early-executed tool results immediately so the model can see them
        for result in &early_results {
            let msg = Message::tool(result.clone());
            self.session.messages.push(msg);
        }

        if tool_calls.is_empty() {
            // No tool calls — turn complete
            turn.transition_to(TurnState::EvaluateContinuation);
            if !turn.budget.should_continue() {
                if turn.recovery_count < self.compact_config.max_recovery_retries {
                    turn.transition_to(TurnState::CompactRecovery);
                    if let Err(e) = self.run_compact_recovery(turn, options, turn_number).await {
                        turn.terminate(TerminalReason::Error(e.clone()));
                        return Ok(TerminalReason::Error(e));
                    }
                    turn.transition_to(TurnState::StartRequest);
                    let (content, _tool_calls, _early_results, usage) =
                        self.send_request_streaming(turn, options).await?;
                    turn.budget.record_tokens(&usage);
                    self.cost_tracker.record_usage(&usage, &self.model);
                    self.last_usage = Some(usage);
                    turn.messages.push(Message::assistant(content));

                    if !turn.budget.should_continue() {
                        turn.terminate(TerminalReason::BudgetExhausted);
                        return Ok(TerminalReason::BudgetExhausted);
                    }

                    turn.transition_to(TurnState::Finalize);
                    self.cost_tracker.end_turn(turn_number, &usage, &self.model);
                    let _ = self.fire_turn_end_hook(turn_number, "end_turn").await;
                    if let Some(reason) = self.record_and_check_loop(&[]) {
                        return Ok(reason);
                    }
                    return Ok(TerminalReason::Completed);
                }
                turn.terminate(TerminalReason::BudgetExhausted);
                return Ok(TerminalReason::BudgetExhausted);
            }
            turn.transition_to(TurnState::Finalize);
            self.cost_tracker.end_turn(turn_number, &usage, &self.model);
            let _ = self.fire_turn_end_hook(turn_number, "end_turn").await;
            if let Some(reason) = self.record_and_check_loop(&[]) {
                return Ok(reason);
            }
            return Ok(TerminalReason::Completed);
        }

        // Phase 5: Execute tools
        turn.pending_tool_calls = tool_calls
            .iter()
            .map(PendingToolCall::from)
            .collect();
        let pending_for_loop = turn.pending_tool_calls.clone();

        turn.transition_to(TurnState::ExecuteTools);
        let tool_results = self.execute_tool_calls(turn, options).await?;

        // Phase 6: Apply tool results
        turn.transition_to(TurnState::ApplyToolResults);
        for result in &tool_results {
            let msg = Message::tool(result.clone());
            self.session.messages.push(msg);
        }

        // Phase 7: Evaluate continuation
        turn.transition_to(TurnState::EvaluateContinuation);
        if !turn.budget.should_continue() {
            if turn.recovery_count < self.compact_config.max_recovery_retries {
                turn.transition_to(TurnState::CompactRecovery);
                if let Err(e) = self.run_compact_recovery(turn, options, turn_number).await {
                    turn.terminate(TerminalReason::Error(e.clone()));
                    self.cost_tracker.end_turn(turn_number, &usage, &self.model);
                    return Ok(TerminalReason::Error(e));
                }
            } else {
                turn.terminate(TerminalReason::BudgetExhausted);
                self.cost_tracker.end_turn(turn_number, &usage, &self.model);
                return Ok(TerminalReason::BudgetExhausted);
            }
        }

        turn.transition_to(TurnState::Finalize);
        self.cost_tracker.end_turn(turn_number, &usage, &self.model);
        let _ = self.fire_turn_end_hook(turn_number, "end_turn").await;

        if let Some(reason) = self.record_and_check_loop(&pending_for_loop) {
            return Ok(reason);
        }
        Ok(TerminalReason::Completed)
    }

    /// Validate structured output if a schema is configured.
    ///
    /// Returns the validated content string. If validation fails and retries
    /// are exhausted, returns the original content with a warning appended.
    fn validate_structured_output(&self, content: String) -> String {
        let Some(ref validator) = self.structured_output else {
            return content;
        };

        // First attempt (retries tracked per-turn)
        let result = validator.validate(&content, 0);
        match result {
            ValidationResult::Valid(_) => content,
            ValidationResult::Invalid { error, retries } => {
                tracing::warn!(
                    "Structured output validation failed: {error} (retry {retries})"
                );
                let warning = format!("\n\n<!-- Structured output validation failed: {error} -->");
                format!("{content}{warning}")
            }
        }
    }

    /// Build the prompt context from session state.
    fn build_context(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
    ) {
        // Assemble context using PromptCompiler
        let ctx = PromptContext {
            cwd: self.session.cwd.as_path(),
            available_tools: &[],
            denied_tools: &self.denied_tools,
            model: &self.model,
            budget: &turn.budget,
        };
        let assembled: AssembledContext = self.prompt_compiler.assemble(&ctx);

        let denied_count = self.denied_tools.len();

        // Prepend context messages (CLAUDE.md wrapped as user message)
        for msg in &assembled.context_messages {
            turn.messages.push(msg.clone());
        }

        // Add system prompt
        turn.messages.push(Message::system(assembled.system_prompt));

        // Add session conversation messages (after system context)
        turn.messages.extend(self.session.messages.iter().cloned());

        // Inject denied tool call summaries from previous turns
        if !self.denied_tools.is_empty() {
            let denied_summary = self.format_denied_tools_summary();
            turn.messages.push(Message::system(denied_summary));
            self.denied_tools.clear();
        }

        // Append custom system prompt if provided
        if let Some(ref prompt) = options.custom_system_prompt {
            turn.messages.push(Message::system(prompt.clone()));
        }

        // Append turn-specific system prompt
        if let Some(ref prompt) = options.append_system_prompt {
            turn.messages.push(Message::system(prompt.clone()));
        }

        // Replay user messages if provided (for session resume)
        if let Some(ref replay) = options.replay_user_messages {
            for msg in replay {
                turn.messages.push(msg.clone());
            }
        }

        tracing::debug!(
            "BuildContext: {} messages, {} estimated tokens ({} denied tools injected)",
            turn.messages.len(),
            assembled.estimated_tokens,
            denied_count,
        );
    }

    /// Run compact recovery when context is too large.
    #[allow(clippy::unused_async)]
    async fn run_compact_recovery(
        &self,
        turn: &mut TurnRuntime,
        _options: &SubmitOptions,
        _turn_number: u32,
    ) -> Result<(), String> {
        // Estimate current token count
        let estimated_tokens = PromptCompiler::estimate_message_tokens(&turn.messages);

        // Select compact strategy
        let stop_reason = "end_turn"; // default — would come from last API response
        let strategy = compact::select_compact_strategy(
            stop_reason,
            estimated_tokens,
            &self.model,
            turn.budget.is_near_limit(),
        );

        // Execute Stop hook with dead loop protection
        let hook_ctx = StopHookContext {
            recovery_count: turn.recovery_count,
            max_recovery_retries: self.compact_config.max_recovery_retries,
            estimated_tokens,
            compact_strategy: strategy,
            stop_reason: stop_reason.to_string(),
        };
        let decision = stop_hook::evaluate_stop_hook(&hook_ctx);

        match decision {
            StopHookDecision::Terminate { reason } => {
                tracing::warn!("Stop hook: {}", reason);
                return Err(reason);
            }
            StopHookDecision::Continue { .. } => {
                tracing::info!(
                    "CompactRecovery: applying {:?} strategy (estimated {} tokens, recovery #{})",
                    strategy,
                    estimated_tokens,
                    turn.recovery_count + 1,
                );
            }
        }

        // Fire PreCompact hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("strategy".to_string(), format!("{strategy:?}"));
        ctx.insert("estimated_tokens".to_string(), estimated_tokens.to_string());
        let _ = self.fire_hook("PreCompact", "compact_recovery", &ctx).await;

        // Apply the compact strategy
        let result: CompactResult = match strategy {
            CompactStrategy::Micro => compact::micro_compact(&turn.messages, 5),
            CompactStrategy::Auto => {
                compact::auto_compact(&turn.messages, self.compact_config.auto_compact_threshold)
            }
            CompactStrategy::Reactive => {
                compact::reactive_compact(&turn.messages, 3)
            }
            CompactStrategy::Snip => {
                compact::snip_compact(&turn.messages, self.compact_config.snip_target_tokens)
            }
        };

        // Check if compact boundary was inserted
        if result.boundary_inserted {
            // Emit compact boundary event
            let _ = self.event_tx.send(EngineEvent::CompactBoundary);
            tracing::debug!("CompactRecovery: boundary marker inserted");
        }

        // Update turn messages with compacted messages
        let messages_remaining = result.messages.len();
        turn.messages = result.messages;

        // Persist compact to transcript file on disk
        if let Some(ref base_path) = self.transcript_path {
            let session_id_str = self.session.session_id.to_string();
            let transcript_file = base_path.join(format!("{session_id_str}.jsonl"));

            // Write compacted messages to transcript
            if let Err(e) = Self::write_transcript_replacement(&transcript_file, &turn.messages) {
                tracing::warn!("Failed to update transcript after compact: {e}");
            }

            // Append boundary marker if one was inserted
            if result.boundary_inserted {
                if let Err(e) = Self::append_transcript_boundary(&transcript_file) {
                    tracing::warn!("Failed to write compact boundary to transcript: {e}");
                }
            }
        }

        // Increment recovery count
        turn.recovery_count += 1;

        // Fire PostCompact hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("tokens_removed".to_string(), result.tokens_removed.to_string());
        ctx.insert("messages_remaining".to_string(), messages_remaining.to_string());
        let _ = self.fire_hook("PostCompact", "compact_complete", &ctx).await;

        tracing::debug!(
            "CompactRecovery: {} tokens removed, {} messages remaining",
            result.tokens_removed,
            turn.messages.len(),
        );

        Ok(())
    }

    /// Write a full replacement to the transcript file with the given messages.
    ///
    /// Uses a temp file + atomic rename for crash safety.
    fn write_transcript_replacement(
        path: &PathBuf,
        messages: &[Message],
    ) -> std::io::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let temp_path = path.with_extension("jsonl.tmp");
        let mut file = std::fs::File::create(&temp_path)?;
        for msg in messages {
            let line = serde_json::to_string(msg)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(file, "{line}")?;
        }
        std::fs::rename(&temp_path, path)
    }

    /// Append a compact boundary marker to the transcript file.
    fn append_transcript_boundary(path: &PathBuf) -> std::io::Result<()> {
        use std::io::Write;

        let boundary = compact::compact_boundary_message();
        let line = serde_json::to_string(&boundary)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }

    /// Format denied tool calls into a system prompt summary.
    fn format_denied_tools_summary(&self) -> String {
        use std::fmt::Write;
        let mut summary =
            String::from("The following tool calls from the previous turn were denied:\n");
        for denied in &self.denied_tools {
            let _ = writeln!(summary, "- `{}`: {}", denied.name, denied.reason);
        }
        summary.push_str("Please do not attempt these tool calls again in the same way. Adjust your approach accordingly.");
        summary
    }

    /// Send the request to the API.
    async fn send_request(
        &mut self,
        turn: &TurnRuntime,
        _options: &SubmitOptions,
    ) -> Result<ApiResponse, String> {
        let system = turn
            .messages
            .iter()
            .filter(|m| matches!(m.role, crate::types::Role::System))
            .filter_map(|m| match &m.content {
                crate::types::MessageContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let messages: Vec<serde_json::Value> = turn
            .messages
            .iter()
            .filter(|m| !matches!(m.role, crate::types::Role::System))
            .filter_map(|m| {
                let role_str = match m.role {
                    crate::types::Role::User => "user",
                    crate::types::Role::Assistant => "assistant",
                    crate::types::Role::Tool => "tool",
                    _ => return None,
                };
                let content = match &m.content {
                    crate::types::MessageContent::Text(t) => serde_json::Value::String(t.clone()),
                    crate::types::MessageContent::ToolCall(tc) => serde_json::json!([
                        {"type": "tool_use", "id": &tc.id, "name": &tc.name, "input": &tc.input}
                    ]),
                    crate::types::MessageContent::ToolResult(tr) => serde_json::json!([
                        {"type": "tool_result", "tool_use_id": &tr.tool_call_id, "content": &tr.output}
                    ]),
                };
                Some(serde_json::json!({"role": role_str, "content": content}))
            })
            .collect();

        let response = match self
            .api_client
            .create_message(&system, &messages, &self.model, 4096)
            .await
        {
            Ok(resp) => resp,
            Err(crate::api_client::ApiError::RetryExhausted { attempts, last_error_message: _ }) => {
                let msg = format!(
                    "The AI service is temporarily unavailable after {attempts} attempts. Please try again later."
                );
                let _ = self.event_tx.send(EngineEvent::Notification { message: msg.clone() });
                return Err(msg);
            }
            Err(e) => return Err(e.to_string()),
        };

        // Cache successful response for graceful degradation
        self.last_successful_response = Some((response.content.clone(), response.usage));

        // Emit text delta event
        let _ = self
            .event_tx
            .send(EngineEvent::TextDelta(response.content.clone()));

        Ok(response)
    }

    /// Send the request using streaming API.
    ///
    /// Returns the accumulated text content, deferred tool calls (for batch
    /// execution after the stream), early-executed tool results, and usage.
    async fn send_request_streaming(
        &mut self,
        turn: &TurnRuntime,
        options: &SubmitOptions,
    ) -> Result<(String, Vec<ToolCall>, Vec<ToolResult>, UsageSummary), String> {
        use futures::StreamExt;
        use std::collections::HashSet;

        let system = turn
            .messages
            .iter()
            .filter(|m| matches!(m.role, crate::types::Role::System))
            .filter_map(|m| match &m.content {
                crate::types::MessageContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let messages: Vec<serde_json::Value> = turn
            .messages
            .iter()
            .filter(|m| !matches!(m.role, crate::types::Role::System))
            .filter_map(|m| {
                let role_str = match m.role {
                    crate::types::Role::User => "user",
                    crate::types::Role::Assistant => "assistant",
                    crate::types::Role::Tool => "tool",
                    _ => return None,
                };
                let content = match &m.content {
                    crate::types::MessageContent::Text(t) => serde_json::Value::String(t.clone()),
                    crate::types::MessageContent::ToolCall(tc) => serde_json::json!([
                        {"type": "tool_use", "id": &tc.id, "name": &tc.name, "input": &tc.input}
                    ]),
                    crate::types::MessageContent::ToolResult(tr) => serde_json::json!([
                        {"type": "tool_result", "tool_use_id": &tr.tool_call_id, "content": &tr.output}
                    ]),
                };
                Some(serde_json::json!({"role": role_str, "content": content}))
            })
            .collect();

        let mut stream = self
            .api_client
            .create_message_streaming(&system, &messages, &self.model, 4096)
            .await;

        let mut assembler = StreamingToolCallAssembler::new();
        let mut usage = UsageSummary {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let mut text_so_far = String::new();
        let mut early_results: Vec<ToolResult> = Vec::new();
        let mut deferred_calls: Vec<ToolCall> = Vec::new();

        while let Some(event) = stream.next().await {
            match event.clone() {
                StreamEvent::TextDelta(ref text) => {
                    text_so_far.push_str(text);
                    let _ = self.event_tx.send(EngineEvent::TextDelta(text.clone()));
                }
                StreamEvent::ToolCallBlockStart { ref id, ref name } => {
                    let _ = self.event_tx.send(EngineEvent::ToolCallStart {
                        id: id.clone(),
                        name: name.clone(),
                    });
                }
                StreamEvent::ToolCallBlockEnd { .. } => {
                    // Completion handled below via assembler.process_event
                }
                StreamEvent::ToolCallInputDelta { ref id, ref delta } => {
                    tracing::trace!("tool call input delta for {id}: {} bytes", delta.len());
                }
                StreamEvent::UsageSummary(ref u) => {
                    usage = u.clone();
                }
                StreamEvent::StreamEnd => {
                    // Finalize pending tool calls — handled after loop
                }
                StreamEvent::Error(ref e) => {
                    tracing::warn!(
                        "streaming network error ({} bytes received so far): {e}",
                        text_so_far.len()
                    );
                    // Fail-open: stop consuming the stream but return whatever
                    // partial text and tool calls we have gathered so far.
                    break;
                }
            }

            // Process event through assembler and execute completed tool calls
            // immediately while the stream is still running (early execution).
            for call in assembler.process_event(event) {
                match self.execute_early_tool_call(call, options).await {
                    Ok(Some(result)) => early_results.push(result),
                    Ok(None) => { /* deferred — call already tracked */ }
                    Err(e) => {
                        tracing::warn!("early tool execution failed: {e}");
                    }
                }
            }
        }

        // Finalize any remaining pending calls from stream end
        for call in assembler.process_event(StreamEvent::StreamEnd) {
            match self.execute_early_tool_call(call, options).await {
                Ok(Some(result)) => early_results.push(result),
                Ok(None) => { /* deferred */ }
                Err(e) => {
                    tracing::warn!("early tool execution failed: {e}");
                }
            }
        }

        // Partition completed calls: early-executed vs deferred.
        let early_ids: HashSet<String> = early_results
            .iter()
            .map(|r| r.tool_call_id.clone())
            .collect();
        let all_completed = assembler.take_completed();
        for call in all_completed {
            if !early_ids.contains(&call.id) {
                deferred_calls.push(call);
            }
        }

        Ok((text_so_far, deferred_calls, early_results, usage))
    }

    /// Execute a single tool call immediately during streaming.
    ///
    /// Runs the full validation pipeline (allowed_tools_override, PreToolUse
    /// hook, PermissionEngine) and executes the tool if allowed.
    ///
    /// - `Ok(Some(result))` — tool was executed or denied immediately.
    /// - `Ok(None)` — tool requires user interaction (Ask/AskForSession/Elevated);
    ///   the caller should defer it for batch execution after streaming.
    async fn execute_early_tool_call(
        &mut self,
        call: ToolCall,
        options: &SubmitOptions,
    ) -> Result<Option<ToolResult>, String> {
        // 1. allowed_tools_override
        if let Some(ref allowed) = options.allowed_tools_override {
            if !allowed.iter().any(|t| t.eq_ignore_ascii_case(&call.name)) {
                self.denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: "tool is not in the allowed tools override".to_string(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!(
                        "tool '{}' blocked: not in allowed tools override",
                        call.name
                    ),
                    is_error: true,
                };
                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                return Ok(Some(result));
            }
        }

        // 2. PreToolUse hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("tool_call_id".to_string(), call.id.clone());
        ctx.insert("tool_input".to_string(), call.input.to_string());
        let pre_hook = self.hooks.fire_hook("PreToolUse", &call.name, &ctx).await;
        if pre_hook.stop {
            self.denied_tools.push(DeniedToolCall {
                name: call.name.clone(),
                reason: "pre-use hook stopped execution".to_string(),
            });
            let result = ToolResult {
                tool_call_id: call.id.clone(),
                output: "tool execution stopped by hook".to_string(),
                is_error: true,
            };
            let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                id: call.id.clone(),
                result: result.clone(),
            });
            return Ok(Some(result));
        }

        // 3. Permission check
        let (_request, perm_result) = self
            .permission_engine
            .evaluate(
                &call.name,
                call.input.to_string(),
                None,
                std::collections::HashMap::new(),
            )
            .await
            .map_err(|e| format!("permission check failed: {e}"))?;

        match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                let outcome = self.tools.execute(&call.id, &call.name, &call.input).await;
                let result = outcome.result;

                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("tool_output_len".to_string(), result.output.len().to_string());
                let _ = self.hooks.fire_hook("PostToolUse", &call.name, &ctx).await;

                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                Ok(Some(result))
            }
            PermissionResult::Ask { reason } | PermissionResult::AskForSession { reason } => {
                let _ = self.event_tx.send(EngineEvent::PermissionRequest {
                    id: call.id.clone(),
                    tool: call.name.clone(),
                    args: call.input.clone(),
                });
                self.denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' denied by user", call.name),
                    is_error: true,
                };
                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let _ = self.hooks.fire_hook("PostToolUseFail", &call.name, &ctx).await;
                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                Ok(Some(result))
            }
            PermissionResult::Deny { reason } => {
                self.denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' denied", call.name),
                    is_error: true,
                };
                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let _ = self.hooks.fire_hook("PostToolUseFail", &call.name, &ctx).await;
                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                Ok(Some(result))
            }
            PermissionResult::Elevated { reason } => {
                let elevation_id = format!("elev_{}", call.id);
                let _ = self.event_tx.send(EngineEvent::ElevationRequest {
                    id: elevation_id.clone(),
                    tool: call.name.clone(),
                    reason: reason.clone(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' requires elevated permissions", call.name),
                    is_error: true,
                };
                let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                Ok(Some(result))
            }
        }
    }

    /// Execute pending tool calls with permission checks.
    async fn execute_tool_calls(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
    ) -> Result<Vec<ToolResult>, String> {
        let outcome = tool_scheduler::run_tool_calls(
            &turn.pending_tool_calls,
            self.tools.as_ref(),
            &mut self.permission_engine,
            self.hooks.as_ref(),
            options,
            &self.event_tx,
            &mut self.denied_tools,
        )
        .await?;

        if let Some(elevation) = outcome.pending_elevation {
            turn.pending_elevation = Some(elevation);
        }
        turn.pending_tool_calls.clear();
        Ok(outcome.results)
    }

    /// Resolve a pending elevation request and retry the tool call with elevated permissions.
    ///
    /// After the user approves elevation, this temporarily grants bypass-level
    /// permissions for the specific tool call and re-executes it.
    pub async fn resolve_elevation(
        &mut self,
        turn: &mut TurnRuntime,
        approved: bool,
    ) -> Result<Option<ToolResult>, String> {
        let Some(elevation) = turn.pending_elevation.take() else {
            return Ok(None);
        };

        if !approved {
            // User denied elevation — treat as permanent denial
            self.denied_tools.push(DeniedToolCall {
                name: elevation.tool_name.clone(),
                reason: "elevation denied by user".to_string(),
            });
            return Ok(Some(ToolResult {
                tool_call_id: elevation.tool_call_id.clone(),
                output: format!("tool '{}' elevation denied", elevation.tool_name),
                is_error: true,
            }));
        }

        // Temporarily switch to BypassPermissions for the elevated call
        let checker = self.permission_engine.checker();
        let prev_mode = {
            let mut c = checker.lock().unwrap();
            let prev = c.mode();
            c.set_mode(PermissionMode::BypassPermissions);
            prev
        };

        // Emit tool call start
        let _ = self.event_tx.send(EngineEvent::ToolCallStart {
            id: elevation.tool_call_id.clone(),
            name: elevation.tool_name.clone(),
        });

        // Fire PreToolUse hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("tool_call_id".to_string(), elevation.tool_call_id.clone());
        ctx.insert("tool_input".to_string(), elevation.input.to_string());
        let pre_hook = self.fire_hook("PreToolUse", &elevation.tool_name, &ctx).await;
        if pre_hook.stop {
            // Restore mode
            {
                let mut c = checker.lock().unwrap();
                c.set_mode(prev_mode);
            }
            return Ok(Some(ToolResult {
                tool_call_id: elevation.tool_call_id.clone(),
                output: "elevated tool execution stopped by hook".to_string(),
                is_error: true,
            }));
        }

        // Re-run permission check (should now pass with BypassPermissions)
        let (_request, perm_result) = self
            .permission_engine
            .evaluate(
                &elevation.tool_name,
                elevation.input.to_string(),
                None,
                std::collections::HashMap::new(),
            )
            .await
            .map_err(|e| format!("permission check failed: {e}"))?;

        // Restore original mode
        {
            let mut c = checker.lock().unwrap();
            c.set_mode(prev_mode);
        }

        let result = match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                let outcome = self
                    .tools
                    .execute(&elevation.tool_call_id, &elevation.tool_name, &elevation.input)
                    .await;
                outcome.result
            }
            _ => {
                self.denied_tools.push(DeniedToolCall {
                    name: elevation.tool_name.clone(),
                    reason: "elevated permission check still denied".to_string(),
                });
                ToolResult {
                    tool_call_id: elevation.tool_call_id.clone(),
                    output: format!("tool '{}' elevated execution denied", elevation.tool_name),
                    is_error: true,
                }
            }
        };

        let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
            id: elevation.tool_call_id.clone(),
            result: result.clone(),
        });
        Ok(Some(result))
    }

    /// Get the current cost tracker (shared reference).
    pub const fn cost_tracker(&self) -> &CostTracker {
        &self.cost_tracker
    }

    /// Get the current turn number.
    #[must_use]
    pub const fn current_turn(&self) -> u32 {
        self.current_turn
    }

    /// Get the session state (read-only).
    pub const fn session(&self) -> &SessionState {
        &self.session
    }

    /// Interrupt the current turn (e.g., from Ctrl+C).
    pub fn interrupt(&self) {
        // Signal the event loop to stop
        let _ = self.event_tx.send(EngineEvent::SessionEnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::StreamEvent;
    use crate::permission_engine::PermissionEngine;
    use crate::permissions::{PermissionChecker, PermissionMode};

    fn make_test_engine() -> (
        QueryEngine,
        mpsc::UnboundedReceiver<EngineEvent>,
        SessionState,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let api = MockApiClient;
        let mut engine =
            QueryEngine::new(Box::new(api), ModelId::sonnet(), session.clone(), perm_engine, tx);
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        (engine, rx, session)
    }

    /// Test-only tool executor that echoes back a successful result so
    /// permission/elevation flow tests can observe the allow path.
    struct StubOkToolExecutor;

    #[async_trait::async_trait]
    impl crate::tool_executor::ToolExecutor for StubOkToolExecutor {
        async fn execute(
            &self,
            tool_call_id: &str,
            tool_name: &str,
            _input: &serde_json::Value,
        ) -> crate::tool_executor::ToolDispatchOutcome {
            crate::tool_executor::ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: format!("stub ok: {tool_name}"),
                    is_error: false,
                },
            }
        }
    }

    // ─── Mock API Client ─────────────────────────────────────────────

    struct MockApiClient;

    #[async_trait::async_trait]
    impl ApiClient for MockApiClient {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(MockApiClient)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: "Hello from the mock assistant.".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 100,
                    output_tokens: 50,
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
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>,
        > {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(StreamEvent::StreamEnd);
            Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }))
        }
    }

    #[test]
    fn turn_state_user_waiting_only_on_specific_states() {
        assert!(TurnState::AwaitPermission.is_user_waiting());
        assert!(TurnState::AwaitElicitation.is_user_waiting());
        assert!(!TurnState::Initialize.is_user_waiting());
        assert!(!TurnState::StreamAssistant.is_user_waiting());
    }

    #[test]
    fn turn_state_terminal_only_on_terminated() {
        assert!(TurnState::Terminated.is_terminal());
        assert!(!TurnState::Finalize.is_terminal());
        assert!(!TurnState::CompactRecovery.is_terminal());
    }

    #[test]
    fn terminal_reason_display_formats() {
        assert_eq!(format!("{}", TerminalReason::Completed), "completed");
        assert_eq!(
            format!("{}", TerminalReason::Error("something broke".into())),
            "error: something broke"
        );
        assert_eq!(
            format!("{}", TerminalReason::MaxTurnsReached),
            "max turns reached"
        );
    }

    #[test]
    fn turn_runtime_new_starts_at_initialize() {
        let budget = TokenBudget::with_defaults();
        let turn = TurnRuntime::new(budget);
        assert_eq!(turn.state, TurnState::Initialize);
        assert!(turn.messages.is_empty());
        assert!(turn.pending_tool_calls.is_empty());
        assert!(turn.pending_permission.is_none());
        assert!(turn.terminal_reason.is_none());
    }

    #[test]
    fn turn_runtime_transition_to_works() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.transition_to(TurnState::BuildContext);
        assert_eq!(turn.state, TurnState::BuildContext);
    }

    #[test]
    fn turn_runtime_terminate_sets_reason() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.terminate(TerminalReason::UserInterrupted);
        assert_eq!(turn.state, TurnState::Terminated);
        assert!(matches!(
            turn.terminal_reason,
            Some(TerminalReason::UserInterrupted)
        ));
    }

    #[test]
    fn turn_runtime_reset_clears_turn_data() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.messages.push(Message::user("hello"));
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        });
        turn.budget.record_tokens(&UsageSummary {
            input_tokens: 0,
            output_tokens: 500,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });

        turn.reset();

        assert_eq!(turn.state, TurnState::Initialize);
        assert!(turn.pending_tool_calls.is_empty());
        assert_eq!(turn.budget.turn_tokens, 0);
        // Messages are preserved on reset (they belong to the session)
    }

    #[test]
    fn submit_options_defaults_are_sane() {
        let opts = SubmitOptions::default();
        assert_eq!(opts.max_turns, 0);
        assert_eq!(opts.max_budget_usd, 0.0);
        assert!(opts.json_schema.is_none());
        assert!(opts.custom_system_prompt.is_none());
        assert!(opts.handle_elicitation);
    }


    #[test]
    fn build_context_loads_session_messages() {
        let (engine, _rx, _session) = make_test_engine();
        // Verify that session() returns a reference to the session
        let session = engine.session();
        assert!(session.messages.is_empty());
        // New sessions start with no messages, build_context would copy them
    }

    #[tokio::test]
    async fn send_request_returns_mock_response() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.messages.push(Message::user("hello"));
        let opts = SubmitOptions::default();

        let result = engine.send_request(&turn, &opts).await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert!(!resp.content.is_empty());
        assert!(resp.usage.output_tokens > 0);
    }

    #[tokio::test]
    async fn execute_tool_calls_with_bypass_permission() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_mock".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "test.txt"}),
        });
        let opts = SubmitOptions::default();

        let results = engine.execute_tool_calls(&mut turn, &opts).await;
        assert!(results.is_ok());
        let results = results.unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error);
    }

    #[tokio::test]
    async fn api_unavailable_returns_graceful_reason() {
        struct FailingMock;

        #[async_trait::async_trait]
        impl ApiClient for FailingMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(FailingMock)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(crate::api_client::ApiError::RetryExhausted {
                    attempts: 3,
                    last_error_message: "connection refused".into(),
                })
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let _ = tx.send(StreamEvent::StreamEnd);
                Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                }))
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let mut engine = QueryEngine::new(
            Box::new(FailingMock),
            ModelId::sonnet(),
            session,
            perm_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));

        let mut options = SubmitOptions::default();
        options.handle_elicitation = false;

        let result = engine.submit("hello".into(), options).await;

        assert!(
            matches!(result, Ok(TerminalReason::ApiUnavailable { .. })),
            "expected ApiUnavailable, got {result:?}"
        );

        // Verify a Notification event was emitted
        let mut found_notification = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, EngineEvent::Notification { .. }) {
                found_notification = true;
                break;
            }
        }
        assert!(found_notification, "expected Notification event to be emitted");
    }

    #[tokio::test]
    async fn execute_tool_calls_respects_allowed_tools_override() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_blocked".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "blocked.txt"}),
        });

        let opts = SubmitOptions {
            allowed_tools_override: Some(vec!["read".into()]),
            ..SubmitOptions::default()
        };

        let results = engine.execute_tool_calls(&mut turn, &opts).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(results[0].output.contains("blocked"));
    }

    #[test]
    fn pending_tool_call_from_tool_call() {
        let tc = ToolCall {
            id: "tc_x".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "out.txt", "content": "hi"}),
        };
        let pending = PendingToolCall::from(&tc);
        assert_eq!(pending.id, "tc_x");
        assert_eq!(pending.name, "write");
        assert_eq!(pending.input["path"], "out.txt");
    }

    #[test]
    fn turn_runtime_recovery_count_starts_at_zero() {
        let budget = TokenBudget::with_defaults();
        let turn = TurnRuntime::new(budget);
        assert_eq!(turn.recovery_count, 0);
        assert!(!turn.stop_hook_active);
    }

    // ─── Structured Output Validation ─────────────────────────────────────

    #[test]
    fn structured_output_validator_validates_valid_json() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "number" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let output = r#"{"name": "Alice", "age": 30}"#;
        let result = validator.validate(output, 0);
        assert!(matches!(result, ValidationResult::Valid(_)));
    }

    #[test]
    fn structured_output_validator_rejects_invalid_json() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let result = validator.validate("not json at all", 0);
        assert!(matches!(result, ValidationResult::Invalid { .. }));
    }

    #[test]
    fn structured_output_validator_rejects_missing_required_field() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name", "email"],
            "properties": {
                "name": { "type": "string" },
                "email": { "type": "string" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let result = validator.validate(r#"{"name": "Alice"}"#, 0);
        assert!(matches!(result, ValidationResult::Invalid { .. }));
    }

    #[test]
    fn structured_output_validator_rejects_wrong_type() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "number" }
            }
        });
        let validator = StructuredOutputValidator::new(schema);
        let result = validator.validate(r#"{"count": "not a number"}"#, 0);
        assert!(matches!(result, ValidationResult::Invalid { .. }));
    }

    #[test]
    fn structured_output_validator_respects_max_retries() {
        let schema = serde_json::json!({ "type": "object" });
        let validator = StructuredOutputValidator::new(schema)
            .with_max_retries(2);
        let result = validator.validate("bad", 2);
        assert!(matches!(result, ValidationResult::Invalid { retries, .. } if retries == 2));
    }

    #[test]
    fn structured_output_validator_retry_instruction() {
        let schema = serde_json::json!({ "type": "object" });
        let validator = StructuredOutputValidator::new(schema);
        let instruction = validator.retry_instruction("missing required field: name");
        assert!(instruction.contains("missing required field: name"));
        assert!(instruction.contains("JSON schema"));
    }

    // ─── Permission Denial Tracking ─────────────────────────────────────

    #[test]
    fn query_engine_tracks_denied_tools() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.denied_tools.push(DeniedToolCall {
            name: "bash".into(),
            reason: "requires approval".into(),
        });
        assert_eq!(engine.denied_tools.len(), 1);
        assert_eq!(engine.denied_tools[0].name, "bash");
    }

    #[test]
    fn query_engine_denied_tools_summary_format() {
        let (engine, _rx, _session) = make_test_engine();
        let summary = engine.format_denied_tools_summary();
        assert!(summary.contains("The following tool calls"));
        assert!(summary.contains("denied"));

        // With denied tools
        let (mut engine, _rx, _session) = make_test_engine();
        engine.denied_tools.push(DeniedToolCall {
            name: "write".into(),
            reason: "permission denied".into(),
        });
        let summary = engine.format_denied_tools_summary();
        assert!(summary.contains("`write`"));
        assert!(summary.contains("permission denied"));
    }

    #[test]
    fn query_engine_clear_denied_tools() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.denied_tools.push(DeniedToolCall {
            name: "test".into(),
            reason: "test".into(),
        });
        engine.clear_denied_tools();
        assert!(engine.denied_tools().is_empty());
    }

    // ─── Elevation Flow ────────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_elevation_approved_succeeds_with_bypass() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_elevation = Some(PendingElevation {
            tool_call_id: "tc_elev".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({"command": "whoami"}),
            request_id: "elev_tc_elev".into(),
        });

        let result = engine.resolve_elevation(&mut turn, true).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_some());
        let r = result.unwrap();
        assert!(!r.is_error);
    }

    #[tokio::test]
    async fn resolve_elevation_denied_returns_error() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_elevation = Some(PendingElevation {
            tool_call_id: "tc_elev".into(),
            tool_name: "write".into(),
            input: serde_json::json!({}),
            request_id: "elev_tc_elev".into(),
        });

        let result = engine.resolve_elevation(&mut turn, false).await;
        assert!(result.is_ok());
        let result = result.unwrap().unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn resolve_elevation_no_pending_returns_none() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        let result = engine.resolve_elevation(&mut turn, true).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn pending_elevation_struct_fields() {
        let elev = PendingElevation {
            tool_call_id: "tc_1".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({}),
            request_id: "elev_1".into(),
        };
        assert_eq!(elev.tool_call_id, "tc_1");
        assert_eq!(elev.tool_name, "bash");
    }

    // ─── Streaming Tool Call Assembler ─────────────────────────────────────

    #[test]
    fn streaming_assembler_accumulates_text() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::TextDelta("Hello".into()));
        assembler.process_event(StreamEvent::TextDelta(" world".into()));
        assert_eq!(assembler.text(), "Hello world");
    }

    #[test]
    fn streaming_assembler_assembles_tool_call_from_deltas() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::ToolCallBlockStart {
            id: "tc_1".into(),
            name: "read".into(),
        });
        assembler.process_event(StreamEvent::ToolCallInputDelta {
            id: "tc_1".into(),
            delta: r#"{"path":"test.txt"}"#.into(),
        });
        let completed = assembler.process_event(StreamEvent::ToolCallBlockEnd {
            id: "tc_1".into(),
            input: serde_json::json!({"path": "test.txt"}),
        });
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].name, "read");
        assert_eq!(assembler.has_tool_calls(), true);
    }

    #[test]
    fn streaming_assembler_finalizes_pending_on_stream_end() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::ToolCallBlockStart {
            id: "tc_2".into(),
            name: "bash".into(),
        });
        assembler.process_event(StreamEvent::ToolCallInputDelta {
            id: "tc_2".into(),
            delta: r#"{"command":"ls"}"#.into(),
        });
        // StreamEnd finalizes pending tool calls
        let finalized = assembler.process_event(StreamEvent::StreamEnd);
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].name, "bash");
    }

    #[test]
    fn streaming_assembler_no_tool_calls_for_text_only() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::TextDelta("text only".into()));
        assert!(!assembler.has_tool_calls());
        assert!(assembler.take_completed().is_empty());
    }

    #[test]
    fn streaming_assembler_takes_completed() {
        let mut assembler = StreamingToolCallAssembler::new();
        assembler.process_event(StreamEvent::ToolCallBlockStart {
            id: "tc_1".into(),
            name: "read".into(),
        });
        let _c = assembler.process_event(StreamEvent::ToolCallBlockEnd {
            id: "tc_1".into(),
            input: serde_json::json!({}),
        });
        // take_completed returns already-completed calls
        let taken = assembler.take_completed();
        assert_eq!(taken.len(), 1);
        // Second take is empty
        assert!(assembler.take_completed().is_empty());
    }

    // ─── Elicitation Integration ───────────────────────────────────────────

    #[tokio::test]
    async fn handle_elicitation_stores_response() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        let result = engine.handle_elicitation(
            &mut turn,
            "elic_1".into(),
            "Which approach?".into(),
        ).await;
        // Default handler returns Cancelled
        assert!(matches!(result, ElicitationResult::Cancelled));
        // Turn state should be AwaitElicitation
        assert_eq!(turn.state, TurnState::AwaitElicitation);
    }

    #[test]
    fn query_engine_has_default_elicitation_handler() {
        let (mut engine, _rx, _session) = make_test_engine();
        // Can set a custom handler
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        engine.set_elicitation_handler(Box::new(crate::elicitation::SdkElicitationHandler::new(tx)));
        // Handler was set without error
    }

    #[tokio::test]
    async fn elicitation_heuristic_triggers_in_execute_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct StatefulMock {
            call_count: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl ApiClient for StatefulMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(StatefulMock {
                    call_count: AtomicUsize::new(self.call_count.load(Ordering::SeqCst)),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                let count = self.call_count.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    Ok(ApiResponse {
                        content: "Which approach would you prefer?".into(),
                        tool_calls: Vec::new(),
                        usage: UsageSummary {
                            input_tokens: 100,
                            output_tokens: 50,
                            cache_read_tokens: 0,
                            cache_creation_tokens: 0,
                        },
                        stop_reason: "end_turn".into(),
                    })
                } else {
                    Ok(ApiResponse {
                        content: "Using approach A.".into(),
                        tool_calls: Vec::new(),
                        usage: UsageSummary {
                            input_tokens: 100,
                            output_tokens: 50,
                            cache_read_tokens: 0,
                            cache_creation_tokens: 0,
                        },
                        stop_reason: "end_turn".into(),
                    })
                }
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let _ = tx.send(StreamEvent::StreamEnd);
                Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                }))
            }
        }

        struct TestElicitationHandler;

        #[async_trait::async_trait]
        impl ElicitationHandler for TestElicitationHandler {
            async fn request(
                &self,
                _questions: Vec<crate::elicitation::ElicitationQuestion>,
            ) -> ElicitationResult {
                ElicitationResult::Responded(crate::elicitation::ElicitationResponse {
                    request_id: "elic_1".into(),
                    selected: vec!["Approach A".into()],
                    other_text: None,
                })
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let api = StatefulMock {
            call_count: AtomicUsize::new(0),
        };
        let mut engine = QueryEngine::new(
            Box::new(api),
            ModelId::sonnet(),
            session,
            perm_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        engine.set_elicitation_handler(Box::new(TestElicitationHandler));

        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        let mut options = SubmitOptions::default();
        options.handle_elicitation = true;

        let reason = engine.execute_turn(&mut turn, &options, 1).await.unwrap();

        assert_eq!(reason, TerminalReason::Completed);

        // turn.messages should contain the user clarification and second assistant response
        let user_texts: Vec<String> = turn
            .messages
            .iter()
            .filter_map(|m| match (&m.role, &m.content) {
                (crate::types::Role::User, crate::types::MessageContent::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(user_texts.iter().any(|t| t == "Approach A"), "expected user clarification in turn messages");

        let assistant_texts: Vec<String> = turn
            .messages
            .iter()
            .filter_map(|m| match (&m.role, &m.content) {
                (crate::types::Role::Assistant, crate::types::MessageContent::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(assistant_texts.len(), 2, "expected two assistant messages: question and answer");
        assert_eq!(assistant_texts[1], "Using approach A.");

        // session.messages should contain the user clarification
        assert!(
            engine.session.messages.iter().any(|m| {
                matches!(&m.content, crate::types::MessageContent::Text(t) if t == "Approach A")
            }),
            "expected user clarification in session messages"
        );
    }

    // ─── Transcript Persistence ────────────────────────────────────────────

    #[test]
    fn set_transcript_path_stores_path() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.set_transcript_path(PathBuf::from("/tmp/transcripts"));
        // Path was set without error
        assert!(engine.transcript_path.is_some());
    }

    #[test]
    fn transcript_path_defaults_to_none() {
        let (engine, _rx, _session) = make_test_engine();
        assert!(engine.transcript_path.is_none());
    }

    #[test]
    fn write_transcript_replacement_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        let messages = vec![
            Message::user("hello"),
            Message::assistant("world"),
        ];

        QueryEngine::write_transcript_replacement(&path, &messages).unwrap();
        assert!(path.exists());

        // Verify content
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn write_transcript_replacement_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        // Write initial messages
        QueryEngine::write_transcript_replacement(&path, &[
            Message::user("old1"),
            Message::user("old2"),
            Message::user("old3"),
        ]).unwrap();

        // Replace with fewer messages
        QueryEngine::write_transcript_replacement(&path, &[
            Message::user("new"),
        ]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn append_transcript_boundary_adds_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        QueryEngine::write_transcript_replacement(&path, &[
            Message::user("before"),
        ]).unwrap();

        QueryEngine::append_transcript_boundary(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("coderex_compact_boundary"));
    }
}
