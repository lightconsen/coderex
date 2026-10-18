//! TurnState state machine — lifecycle states, terminal reasons, and transitions.
//!
//! The turn state machine drives a single conversation turn through:
//! 1. Context building (system prompts, memory, MCP, tools)
//! 2. API request (streaming or non-streaming)
//! 3. Tool execution (with permission checks)
//! 4. Continuation evaluation (should we keep going?)
//! 5. Compact/recovery if needed
//! 6. Finalize and emit events

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::api_client::{ApiClient, ApiResponse, RequestContext, StreamEvent};
use crate::budget::TokenBudget;
use crate::compact::{self, CompactResult, CompactStrategy};
use crate::confirmation_bus::{ConfirmationBus, ConfirmationRequest, ConfirmationResponse};
use crate::cost::CostTracker;
use crate::elicitation::{ElicitationHandler, ElicitationResult, ReplElicitationHandler};
use crate::hook_events;
use crate::hook_executor::{HookExecutor, HookFireResult, NoOpHookExecutor};
use crate::loop_detector::{LoopDetection, LoopDetector};
use crate::permission_engine::{PermissionEngine, PermissionResponse};
use crate::permissions::{PermissionChecker, PermissionMode, PermissionResult};
use crate::prompt::{AssembledContext, CompactConfig, PromptCompiler, PromptContext};
use crate::state::SessionState;
use crate::stop_hook::{self, StopHookContext, StopHookDecision};
use crate::tool_executor::{NoOpToolExecutor, ToolExecutor};
use crate::tool_scheduler;
use crate::types::{
    ContextWindowResolver, EngineEvent, Message, ModelId, ToolCall, ToolResult, UsageSummary,
};
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
    /// Ids already completed (success or malformed-completion) — duplicate
    /// `ToolCallBlockEnd`s for the same id (OpenAI Responses emits the final
    /// item twice in some paths) are ignored instead of re-completed.
    completed_ids: HashSet<String>,
    /// Number of tool calls whose accumulated JSON failed to parse.
    malformed_count: usize,
    /// Ids of tool calls whose accumulated JSON failed to parse.
    malformed_ids: Vec<String>,
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
            completed_ids: HashSet::new(),
            malformed_count: 0,
            malformed_ids: Vec::new(),
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
                let Some((name, builder)) = self.pending.remove(&id) else {
                    if self.completed_ids.contains(&id) {
                        // Duplicate BlockEnd for an already-completed call
                        // (OpenAI Responses emits it twice).
                        return Vec::new();
                    }
                    tracing::warn!("tool call block end for unseen id {id} — ignored");
                    return Vec::new();
                };
                let input = if input.is_null() {
                    match serde_json::from_str(&builder) {
                        Ok(v) => v,
                        Err(e) => {
                            // Complete with empty input instead of dropping:
                            // a dropped call leaves a persisted tool_use
                            // without a tool_result, breaking the next
                            // request's message contract. An empty input
                            // flows through the normal pipeline and fails
                            // cleanly (missing required args -> error
                            // ToolResult back to the model).
                            tracing::warn!(
                                "tool call {id} ({name}) has malformed JSON input: {e}\naccumulated: {builder}"
                            );
                            self.malformed_count += 1;
                            self.malformed_ids.push(id.clone());
                            serde_json::Value::Object(Default::default())
                        }
                    }
                } else {
                    input
                };
                self.completed_ids.insert(id.clone());
                let call = ToolCall { id, name, input };
                self.completed.push(call.clone());
                vec![call]
            }
            StreamEvent::StreamEnd => {
                // Finalize any pending tool calls from accumulated JSON
                let mut calls = Vec::new();
                for (id, (name, builder)) in self.pending.drain() {
                    let input = match serde_json::from_str(&builder) {
                        Ok(input) => input,
                        Err(e) => {
                            // Same empty-input completion as BlockEnd: the
                            // message contract must hold.
                            tracing::warn!(
                                "tool call {id} ({name}) has malformed JSON input at stream end: {e}\naccumulated: {builder}"
                            );
                            self.malformed_count += 1;
                            self.malformed_ids.push(id.clone());
                            let call = ToolCall {
                                id: id.clone(),
                                name,
                                input: serde_json::Value::Object(Default::default()),
                            };
                            self.completed.push(call.clone());
                            calls.push(call);
                            continue;
                        }
                    };
                    let call = ToolCall { id, name, input };
                    self.completed.push(call.clone());
                    calls.push(call);
                }
                calls
            }
            StreamEvent::UsageSummary(_) | StreamEvent::Error(_) | StreamEvent::StopReason(_) => {
                Vec::new()
            }
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

    /// Number of tool calls whose accumulated JSON failed to parse.
    #[must_use]
    pub const fn malformed_count(&self) -> usize {
        self.malformed_count
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
    /// The name of this state, for logs.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::BuildContext => "build_context",
            Self::RunPreSubmitHooks => "run_pre_submit_hooks",
            Self::StartRequest => "start_request",
            Self::StreamAssistant => "stream_assistant",
            Self::ExecuteTools => "execute_tools",
            Self::AwaitPermission => "await_permission",
            Self::AwaitElicitation => "await_elicitation",
            Self::ApplyToolResults => "apply_tool_results",
            Self::EvaluateContinuation => "evaluate_continuation",
            Self::CompactRecovery => "compact_recovery",
            Self::StopHooks => "stop_hooks",
            Self::Finalize => "finalize",
            Self::Terminated => "terminated",
        }
    }

    /// Whether this state may be followed by `next`.
    ///
    /// The graph is what a turn actually does:
    ///
    /// ```text
    /// Initialize → BuildContext → RunPreSubmitHooks → StartRequest → StreamAssistant
    /// StreamAssistant → ExecuteTools → ApplyToolResults → EvaluateContinuation
    /// StreamAssistant → EvaluateContinuation            (no tools to run)
    /// EvaluateContinuation → CompactRecovery → StartRequest   (context too large)
    /// CompactRecovery / StartRequest → StopHooks              (recovery gave up)
    /// EvaluateContinuation → StopHooks → Finalize → Terminated
    /// ExecuteTools → AwaitPermission ─┐                 (a decision is needed)
    /// StartRequest → AwaitElicitation ┘→ back where they came from
    /// ```
    ///
    /// Any state may be terminated, and any state may be re-entered from
    /// `Initialize` for a new turn.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        // A turn can always end, and a new turn always starts the same way.
        if matches!(next, Self::Terminated) || matches!(self, Self::Initialize) {
            return matches!(next, Self::Terminated | Self::BuildContext);
        }
        match self {
            Self::Initialize => unreachable!(),
            Self::BuildContext => matches!(next, Self::RunPreSubmitHooks),
            // RunPreSubmitHooks → CompactRecovery is the cache-TTL pass: it
            // runs before the request, while the token line has not been
            // crossed.
            Self::RunPreSubmitHooks => matches!(next, Self::StartRequest | Self::CompactRecovery),
            // A recovery retry is re-requested from `StartRequest`; when that
            // retry is still over budget the turn closes from here, so the
            // stop hooks are reachable from this state too.
            Self::StartRequest => matches!(
                next,
                Self::StreamAssistant | Self::AwaitElicitation | Self::StopHooks
            ),
            Self::StreamAssistant => matches!(
                next,
                Self::ExecuteTools | Self::EvaluateContinuation | Self::AwaitElicitation
            ),
            // A permission decision is answered and the tools run.
            Self::AwaitPermission => matches!(next, Self::ExecuteTools | Self::ApplyToolResults),
            Self::AwaitElicitation => matches!(next, Self::StreamAssistant | Self::StartRequest),
            Self::ExecuteTools => matches!(next, Self::ApplyToolResults | Self::AwaitPermission),
            Self::ApplyToolResults => matches!(next, Self::EvaluateContinuation),
            // The stop hooks run on the way out, from the last decision point.
            Self::EvaluateContinuation => {
                matches!(next, Self::CompactRecovery | Self::StopHooks)
            }
            // A compaction that fails (circuit breaker open / stop hook) ends
            // the turn from `CompactRecovery`, and the closing path enters
            // the stop hooks from here.
            Self::CompactRecovery => {
                matches!(next, Self::StartRequest | Self::StopHooks | Self::Finalize)
            }
            Self::StopHooks => matches!(next, Self::Finalize),
            Self::Finalize => matches!(next, Self::Terminated),
            // Terminating an ended turn is a no-op, not a mistake.
            Self::Terminated => matches!(next, Self::Terminated),
        }
    }

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
    /// The provider paused the turn (`pause_turn`/`compaction`) and asked for
    /// the same request again. Not an end: the loop re-sends with the same
    /// context, and this is the signal that says so.
    Resume,
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
            Self::LoopDetected {
                period,
                repetitions,
            } => {
                write!(
                    f,
                    "loop detected (period={period}, repetitions={repetitions})"
                )
            }
            Self::ApiUnavailable { message } => {
                write!(f, "api unavailable: {message}")
            }
            Self::Resume => write!(f, "resumed"),
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
    /// Why this call needs an explicit elevation decision.
    pub reason: String,
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
    /// Optional agent id for forked/background flows.
    pub agent_id: Option<String>,
    /// Provider-specific request hints.
    pub provider_hints: HashMap<String, String>,
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
            agent_id: None,
            provider_hints: HashMap::new(),
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
    /// Consecutive compact failures (circuit breaker for auto-compaction).
    pub compact_failures: u32,
    /// Whether the stop hook is active.
    pub stop_hook_active: bool,
    /// The agent this turn belongs to, when it is a forked or background run.
    ///
    /// Permission decisions read this: a background run is one the person is
    /// not watching, which is a different question from the one they answer
    /// interactively.
    pub agent_id: Option<String>,
    /// Terminal reason (set when state transitions to Terminated).
    pub terminal_reason: Option<TerminalReason>,
    /// The last provider stop reason, for compaction strategy selection.
    ///
    /// `Reactive` compaction is for a response the model ran out of room for;
    /// that fact lives on the response, not in the context estimate.
    pub last_stop_reason: Option<String>,
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
            compact_failures: 0,
            stop_hook_active: false,
            agent_id: None,
            terminal_reason: None,
            last_stop_reason: None,
        }
    }

    /// Reset for a new turn.
    ///
    /// `messages` is the per-request message list, rebuilt from scratch by
    /// `build_context` each turn. It must be cleared here: the turn runtime is
    /// reused across the whole `submit_streaming` loop, so a list left in place
    /// would be extended again on every turn — duplicating the context
    /// preamble, the system prompt, and the entire session history once per
    /// turn, and growing the request body superlinearly.
    pub fn reset(&mut self) {
        self.state = TurnState::Initialize;
        self.messages.clear();
        self.pending_tool_calls.clear();
        self.pending_permission = None;
        self.pending_elevation = None;
        self.pending_elicitation = None;
        self.last_stop_reason = None;
        self.budget.reset_turn();
    }

    /// Transition to a new state, reporting a move the graph does not allow.
    ///
    /// The state is a diagnostic — what the engine is doing, for the UI and for
    /// whoever reads a log — and this used to be a bare assignment, so any
    /// state could follow any other and the value meant nothing. The legal
    /// moves are in [`TurnState::can_transition_to`].
    ///
    /// An illegal move is reported rather than refused: refusing would turn a
    /// bookkeeping mistake into a failed turn, and the work is not what is
    /// wrong.
    pub fn transition_to(&mut self, new_state: TurnState) {
        #[cfg(test)]
        record_transition(self.state, new_state);

        if !self.state.can_transition_to(new_state) {
            tracing::warn!(
                from = self.state.as_str(),
                to = new_state.as_str(),
                "the turn moved between states the graph does not allow"
            );
        }
        self.state = new_state;
    }

    /// Terminate with a reason.
    pub fn terminate(&mut self, reason: TerminalReason) {
        self.state = TurnState::Terminated;
        self.terminal_reason = Some(reason);
    }
}

/// Why a turn could not get a response from the model.
///
/// The turn loop used to tell these apart by reading the message —
/// `Err(e) if e.starts_with("The AI service is temporarily unavailable")` —
/// which meant the two halves of the engine were joined by a sentence: change
/// the wording where it is produced and the loop quietly stops recognising it,
/// reporting a provider outage as a plain error instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnError {
    /// The provider could not be reached, after retrying. The same request may
    /// work later, and the session is intact.
    ApiUnavailable {
        /// What to show a person.
        message: String,
    },
    /// Anything else: a bad request, a refusal, a bug here.
    Fatal {
        /// What to show a person.
        message: String,
    },
}

impl TurnError {
    /// The message a person sees.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::ApiUnavailable { message } | Self::Fatal { message } => message,
        }
    }

    /// The reason this turn ended.
    #[must_use]
    pub fn terminal_reason(&self) -> TerminalReason {
        match self {
            Self::ApiUnavailable { message } => TerminalReason::ApiUnavailable {
                message: message.clone(),
            },
            Self::Fatal { message } => TerminalReason::Error(message.clone()),
        }
    }

    /// The same error as a plain message.
    #[must_use]
    pub fn into_message(self) -> String {
        match self {
            Self::ApiUnavailable { message } | Self::Fatal { message } => message,
        }
    }
}

impl From<String> for TurnError {
    /// A message that is not a provider outage — a tool that could not run, a
    /// hook that failed — is the engine's own trouble.
    fn from(message: String) -> Self {
        Self::Fatal { message }
    }
}

impl From<TurnError> for String {
    fn from(error: TurnError) -> Self {
        error.into_message()
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

// Every transition a turn made, for the test that checks the graph against
// what the engine actually does.
#[cfg(test)]
thread_local! {
    static RECORDED_TRANSITIONS: std::cell::RefCell<Vec<(TurnState, TurnState)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_transition(from: TurnState, to: TurnState) {
    RECORDED_TRANSITIONS.with(|recorded| recorded.borrow_mut().push((from, to)));
}

#[cfg(test)]
fn take_recorded_transitions() -> Vec<(TurnState, TurnState)> {
    RECORDED_TRANSITIONS.with(|recorded| std::mem::take(&mut *recorded.borrow_mut()))
}

/// What one streamed request produced.
///
/// The stop reason is part of it: the blocking path has always had it on the
/// response, and the streaming path used to drop it — which is why a truncated
/// answer looked like a finished turn.
#[derive(Debug, Clone)]
struct StreamedResponse {
    content: String,
    tool_calls: Vec<ToolCall>,
    /// Calls that were executed while the stream was still running.
    ///
    /// Their `tool_use` messages still have to be written to the session: the
    /// assistant turn and its `tool_result` are a pair, and a history with the
    /// result but not the call is rejected by the provider.
    early_calls: Vec<ToolCall>,
    early_results: Vec<ToolResult>,
    usage: UsageSummary,
    stop_reason: String,
}

/// Whether a provider's stop reason means "the answer was cut off".
///
/// Providers spell it two ways: `max_tokens` is Anthropic's, `length` is
/// OpenAI's. Reading only one of them means a truncated answer from the other
/// provider is reported as a finished turn.
#[must_use]
fn stop_reason_eq(stop_reason: &str, expected: &str) -> bool {
    match expected {
        "max_tokens" => stop_reason == "max_tokens" || stop_reason == "length",
        other => stop_reason == other,
    }
}

/// Combine text delivered before a retry with the text the retry produced.
///
/// A stream cannot be resumed at a byte offset, so a retry re-requests the
/// turn from the beginning and the model regenerates the opening it already
/// produced. The two texts therefore share a prefix, and taking the longer one
/// keeps the reader from being shown the same sentence twice. When the retry
/// instead picks up past where the first attempt stopped — or diverges — both
/// are kept, so nothing already delivered is lost.
///
/// This is the all-at-once form, kept as the reference behaviour for tests.
/// The streaming loop uses [`StreamTextMerger`], which reaches the same result
/// one delta at a time.
#[cfg(test)]
#[must_use]
fn merge_stream_text(delivered: &str, retried: &str) -> String {
    if retried.starts_with(delivered) {
        retried.to_string()
    } else if delivered.starts_with(retried) {
        delivered.to_string()
    } else {
        let mut merged = String::with_capacity(delivered.len() + retried.len());
        merged.push_str(delivered);
        merged.push_str(retried);
        merged
    }
}

/// Incrementally merge the text of a retried stream.
///
/// A retry regenerates the answer from the beginning. Comparing the whole
/// accumulated attempt against the whole delivered text on every delta — as
/// the loop used to — re-applied the repeated opening once per delta, so a
/// divergent retry emitted `x`, then `xy`, then `xyz` instead of the three
/// fragments once, and the output grew quadratically.
///
/// This tracks the attempt separately from what the reader has seen. Each
/// delta is examined exactly once: it is skipped while it still reproduces
/// the delivered prefix, and from the point the retry diverges (or picks up
/// past what was delivered) the new suffix is emitted once and appended to
/// the delivered text.
#[derive(Debug, Default)]
struct StreamTextMerger {
    /// Everything emitted to the reader so far, across attempts.
    delivered: String,
    /// The current attempt's text. A retry starts it again from empty.
    attempt: String,
    /// Set once the attempt stops matching the delivered prefix.
    diverged: bool,
}

impl StreamTextMerger {
    fn new() -> Self {
        Self::default()
    }

    /// Start a fresh attempt. A retry regenerates the answer from the top, so
    /// the comparison with what was delivered starts over.
    fn begin_attempt(&mut self) {
        self.attempt.clear();
        self.diverged = false;
    }

    /// The text emitted to the reader so far.
    fn delivered(&self) -> &str {
        &self.delivered
    }

    /// Finish streaming, returning the full delivered text.
    fn into_delivered(self) -> String {
        self.delivered
    }

    /// Merge one delta from the current attempt.
    ///
    /// Returns only the bytes the reader has not seen yet, or `None` when the
    /// delta merely repeats the already-delivered opening.
    fn push_delta(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }

        let start = self.attempt.len();
        self.attempt.push_str(delta);

        if self.diverged {
            self.delivered.push_str(delta);
            return Some(delta.to_string());
        }

        // Until the attempt diverges, its prefix matches the delivered text
        // byte for byte, so `position` is a char boundary in both strings.
        let mut position = start;
        for ch in delta.chars() {
            match self
                .delivered
                .get(position..)
                .and_then(|s| s.chars().next())
            {
                Some(delivered_ch) if delivered_ch == ch => {
                    position += ch.len_utf8();
                }
                _ => {
                    // The retry has either diverged from the delivered answer
                    // or run past its end. Everything from here is new.
                    self.diverged = true;
                    let new_text = self.attempt[position..].to_string();
                    self.delivered.push_str(&new_text);
                    return Some(new_text);
                }
            }
        }

        None
    }
}

/// What a provider's stop reason asks the turn loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopDisposition {
    /// The model wants tools run before it can go on.
    RunTools,
    /// The provider paused the turn and expects the same request again: it
    /// ran out of a server-side budget (`pause_turn`) or compacted its own
    /// context (`compaction`). Neither is the end of the turn — treating them
    /// as one silently truncates the answer.
    Resume,
    /// The turn is over, whether the model finished or was cut short.
    Stop,
}

/// Read a provider's stop reason for what it asks the loop to do.
///
/// Anthropic spells the paused cases `pause_turn` and `compaction`; OpenAI
/// compatible providers spell a tool call `tool_calls`. Anything not named
/// here — `end_turn`, `max_tokens`, `length`, `refusal`, `stop`, and whatever
/// a provider invents next — ends the turn, which is the safe default: an
/// unknown reason must not make the loop re-request forever.
#[must_use]
pub fn classify_stop_reason(stop_reason: &str) -> StopDisposition {
    match stop_reason {
        "tool_use" | "tool_calls" => StopDisposition::RunTools,
        "pause_turn" | "compaction" => StopDisposition::Resume,
        _ => StopDisposition::Stop,
    }
}

// ─── Compaction outcome ─────────────────────────────────────────────────────

/// What asked for compaction to be considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    /// The context crossed the token line.
    TokenThreshold,
    /// The prompt cache went cold while the session sat idle.
    CacheTtl,
}

/// What became of the LLM summary step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// The model wrote the summary.
    Summarized,
    /// The model was not asked — this trigger does not use a summary.
    NotRequested,
    /// There was too little conversation between the preserved head and tail
    /// to be worth a call.
    RangeTooSmall {
        /// Messages available.
        messages: usize,
    },
    /// The summary call failed.
    Failed {
        /// The failure.
        error: String,
    },
    /// The summary came back empty.
    Empty,
}

/// What compaction did, or why it did not happen.
///
/// The engine used to answer this only in `tracing::warn!` strings — a caller
/// cannot act on a log line and a test cannot assert one. Every path through
/// `run_compact_recovery` now returns one of these instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    /// Compaction ran and removed tokens.
    Compacted {
        /// Which strategy ran.
        strategy: CompactStrategy,
        /// What triggered it.
        trigger: CompactTrigger,
        /// Estimated tokens removed.
        tokens_removed: usize,
        /// Messages left after compaction.
        messages_remaining: usize,
        /// What became of the summary step.
        summary: SummaryOutcome,
    },
    /// The check ran but this trigger stood down: the context is not in the
    /// band this trigger acts on, and the other path owns it.
    ///
    /// The cache-TTL pass is the one that defers — once the token line has
    /// been crossed, compacting for the threshold is the right response and
    /// doing both would compact twice for one reason.
    DeferredToThresholdPath {
        /// Estimated tokens at the time.
        estimated_tokens: usize,
    },
    /// The recovery retry budget is spent.
    SkippedRecoveryBudget {
        /// Recovery attempts already made.
        recovery_count: u32,
        /// The limit.
        max: u32,
    },
    /// Auto-compaction is paused after consecutive failures.
    CircuitBreakerOpen {
        /// Consecutive failures so far.
        consecutive_failures: u32,
        /// The limit.
        max: u32,
    },
    /// The Stop hook ended the turn instead of compacting.
    StopHookTerminated {
        /// The hook's reason.
        reason: String,
    },
    /// Compaction ran but removed nothing — the context is irrecoverable by
    /// this strategy, and the circuit breaker counts it.
    NoProgress {
        /// Tokens before.
        tokens_before: usize,
        /// Tokens after.
        tokens_after: usize,
        /// What became of the summary step.
        summary: SummaryOutcome,
    },
}

impl CompactOutcome {
    /// The message to terminate the turn with, when this outcome is a failure.
    ///
    /// This is the only failure test the engine uses: every terminal consumer
    /// needs the reason, and `None` is exactly "not a failure".
    #[must_use]
    pub fn failure_message(&self) -> Option<String> {
        match self {
            Self::CircuitBreakerOpen {
                consecutive_failures,
                max,
            } => Some(format!(
                "CompactRecovery: {consecutive_failures}/{max} consecutive compact failures — \
                 stopping auto-retry. Use /compact manually or start a new session."
            )),
            Self::StopHookTerminated { reason } => Some(reason.clone()),
            _ => None,
        }
    }
}

/// How many provider-paused turns the loop will resume before it gives up.
///
/// A provider that answers `pause_turn` forever must not spin the loop
/// forever; past this many consecutive resumes the pause is reported as the
/// end of the run.
pub const MAX_PROVIDER_RESUMES: u32 = 8;

/// What the outer turn loop does after a turn returns a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopStep {
    /// Run another turn.
    Continue,
    /// Stop the run, reporting this reason.
    Break(TerminalReason),
}

/// Decide whether the loop runs another turn after `reason`.
///
/// `last_message_is_tool_result` is the old rule for `Completed`: a turn that
/// ended on a tool result is not finished, the model has not seen the result
/// yet. `resumes` is how many consecutive provider-paused turns have already
/// run, which bounds the `Resume` case.
fn next_loop_step(
    reason: &TerminalReason,
    last_message_is_tool_result: bool,
    resumes: u32,
) -> LoopStep {
    if matches!(reason, TerminalReason::Resume) {
        // A paused turn continues with the same context. The bound keeps a
        // provider that never stops pausing from looping forever.
        return if resumes >= MAX_PROVIDER_RESUMES {
            LoopStep::Break(TerminalReason::Completed)
        } else {
            LoopStep::Continue
        };
    }

    if matches!(
        reason,
        TerminalReason::Completed
            | TerminalReason::TokenLimitStop
            | TerminalReason::LoopDetected { .. }
            | TerminalReason::ApiUnavailable { .. }
    ) {
        if matches!(reason, TerminalReason::Completed) && last_message_is_tool_result {
            LoopStep::Continue
        } else {
            LoopStep::Break(reason.clone())
        }
    } else {
        LoopStep::Continue
    }
}

// ─── Interrupts ─────────────────────────────────────────────────────────────

/// A way to stop the turn an engine is running, from outside it.
///
/// `QueryEngine::interrupt` used to send an event and nothing else: the event
/// was emitted, nothing listened for it, and the turn ran to completion — so
/// Ctrl+C abandoned the *result* while the work carried on. This handle is
/// what a caller outside the engine (a key handler, a parent task) can hold,
/// and what the turn loop actually looks at.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle(Arc<std::sync::atomic::AtomicBool>);

impl InterruptHandle {
    /// Ask the running turn to stop at its next safe point.
    pub fn interrupt(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a stop has been asked for.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Clear the request, so the next turn runs.
    pub fn clear(&self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
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
    /// Whether the running turn has been asked to stop.
    interrupt: InterruptHandle,
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
    /// Explicit context-window override chain (settings/experiment included).
    ///
    /// When absent, compaction uses the default resolver (environment, client
    /// data, built-in table). The CLI installs one built from resolved
    /// settings and experiment flags.
    context_window_resolver: Option<ContextWindowResolver>,
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
    /// Optional persistent session memory that survives compaction.
    /// Implemented by `coderex-memory`; absent → no memory work happens.
    session_memory: Option<Arc<dyn crate::session_memory::SessionMemoryBackend>>,
    /// Optional compaction backend (the local/remote dual-track seam).
    /// Absent → the built-in local LLM summary path is used.
    compaction_backend: Option<Arc<dyn crate::compaction_backend::CompactionBackend>>,
    /// When the last provider request started, for the prompt-cache TTL
    /// trigger: every request refreshes the cache, so the clock starts there.
    last_request_at: Option<Instant>,
    /// Maximum output tokens requested from the provider per request.
    ///
    /// Defaults to [`Self::DEFAULT_MAX_TOKENS`]; a session selected with
    /// `--agent` may lower or raise it from the agent definition.
    max_tokens: u32,
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
    fn persist_assistant_tool_calls(&mut self, turn: &mut TurnRuntime, tool_calls: &[ToolCall]) {
        for tool_call in tool_calls {
            let msg = Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tool_call.clone()),
            };
            turn.messages.push(msg.clone());
            self.session.messages.push(msg);
        }
    }

    fn provider_messages(messages: &[Message]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .filter(|m| !matches!(m.role, crate::types::Role::System))
            .filter_map(|m| {
                let role_str = match (&m.role, &m.content) {
                    (crate::types::Role::User, _)
                    | (crate::types::Role::Tool, crate::types::MessageContent::ToolResult(_)) => {
                        "user"
                    }
                    (crate::types::Role::Assistant, _) => "assistant",
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
            .collect()
    }

    fn build_request_context(&self, turn_number: u32, options: &SubmitOptions) -> RequestContext {
        RequestContext {
            session_id: Some(self.session.session_id.to_string()),
            device_id: std::env::var("CODEREX_DEVICE_ID").ok(),
            turn_id: Some(turn_number.to_string()),
            agent_id: options.agent_id.clone(),
            request_id: None,
            provider_hints: options.provider_hints.clone().into_iter().collect(),
        }
    }

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
            interrupt: InterruptHandle::default(),
            permission_engine,
            event_tx,
            current_turn: 0,
            denied_tools: Vec::new(),
            structured_output: None,
            prompt_compiler: PromptCompiler::new(config),
            compact_config: config,
            context_window_resolver: None,
            hooks: Box::new(NoOpHookExecutor),
            tools: Box::new(NoOpToolExecutor),
            elicitation_handler: Box::new(ReplElicitationHandler::default()),
            transcript_path: None,
            last_usage: None,
            loop_detector: Some(LoopDetector::new()),
            last_successful_response: None,
            team_context: None,
            coordinator: None,
            session_memory: None,
            compaction_backend: None,
            last_request_at: None,
            max_tokens: Self::DEFAULT_MAX_TOKENS,
        }
    }

    /// The default maximum output tokens per provider request.
    pub const DEFAULT_MAX_TOKENS: u32 = 4096;

    /// Set the elicitation handler.
    /// Replace the client requests are sent through.
    ///
    /// Used when a session picks a provider after construction, and by tests
    /// that need a model which behaves a particular way mid-turn.
    pub fn set_api_client(&mut self, client: Box<dyn ApiClient>) {
        self.api_client = client;
    }

    /// Replace the model used for provider requests.
    ///
    /// Used when a named custom agent selected with `--agent` supplies its own
    /// model, and by tests that need to observe the model a request carried.
    pub fn set_model(&mut self, model: ModelId) {
        self.model = model;
    }

    /// The model used for provider requests.
    #[must_use]
    pub fn model(&self) -> &ModelId {
        &self.model
    }

    /// Set the maximum output tokens requested from the provider.
    pub fn set_max_tokens(&mut self, max_tokens: u32) {
        self.max_tokens = max_tokens;
    }

    /// The maximum output tokens requested from the provider.
    #[must_use]
    pub fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    /// Replace the elicitation handler — how the engine asks the person a
    /// question mid-turn.
    pub fn set_elicitation_handler(&mut self, handler: Box<dyn ElicitationHandler + Send + Sync>) {
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

    /// Stop persisting compaction results.
    pub fn clear_transcript_path(&mut self) {
        self.transcript_path = None;
    }

    /// The transcript directory compaction uses, if one is configured.
    #[must_use]
    pub fn transcript_path(&self) -> Option<&std::path::Path> {
        self.transcript_path.as_deref()
    }

    /// Attach a persistent session-memory backend.
    ///
    /// When set (and `CompactConfig::session_memory_enabled`), compaction
    /// refreshes the memory document and injects it alongside the summary.
    pub fn set_session_memory(
        &mut self,
        backend: Arc<dyn crate::session_memory::SessionMemoryBackend>,
    ) {
        self.session_memory = Some(backend);
    }

    /// Attach a compaction backend (the local/remote dual-track seam).
    ///
    /// When set, compaction consults this backend for the summary step
    /// before falling back to the built-in local LLM path; on backend error
    /// or an empty summary the built-in path (then rules) still runs. No
    /// backend attached (the default) → behavior unchanged.
    pub fn set_compaction_backend(
        &mut self,
        backend: Arc<dyn crate::compaction_backend::CompactionBackend>,
    ) {
        self.compaction_backend = Some(backend);
    }

    /// Replace the compaction configuration (thresholds, reserves, retries).
    pub fn set_compact_config(&mut self, config: CompactConfig) {
        self.compact_config = config;
    }

    /// Install a context-window override chain that includes settings and
    /// experiment values.
    ///
    /// The CLI builds this from the resolved `Settings` and `FeatureFlags`;
    /// without it the engine still honors the environment, client-data file,
    /// and built-in table.
    pub fn set_context_window_resolver(&mut self, resolver: ContextWindowResolver) {
        self.context_window_resolver = Some(resolver);
    }

    /// Compaction thresholds for this engine's model and override chain.
    fn compact_thresholds(&self) -> compact::CompactThresholds {
        match &self.context_window_resolver {
            Some(resolver) => compact::CompactThresholds::resolve_with(
                &self.model,
                &self.compact_config,
                resolver,
            ),
            None => compact::CompactThresholds::resolve(&self.model, &self.compact_config),
        }
    }

    /// Refresh the session-memory document at compaction time and return the
    /// summary text with the memory injected.
    ///
    /// No backend attached (or disabled in config) → returns the summary
    /// unchanged with zero extra LLM calls, which keeps existing mock tests
    /// structurally unaffected. On any LLM or backend failure the summary is
    /// returned unchanged — compaction always succeeds, and stale memory is
    /// never injected as if current.
    async fn refresh_session_memory(&self, summary_text: &str, summary_input: &[String]) -> String {
        let backend = match self
            .session_memory
            .as_ref()
            .filter(|_| self.compact_config.session_memory_enabled)
        {
            Some(b) => b,
            None => return summary_text.to_string(),
        };

        let excerpt = summary_input.join("\n\n");
        let session_id = self.session.session_id.to_string();
        let current = backend.load_memory(&session_id).await.unwrap_or_default();

        let update = if current.trim().is_empty() {
            crate::prompts::create_session_memory(self.api_client.as_ref(), &excerpt).await
        } else {
            crate::prompts::update_session_memory(self.api_client.as_ref(), &current, &excerpt)
                .await
        };

        match update {
            Ok(updated) if !updated.trim().is_empty() => {
                backend.save_memory(&session_id, &updated).await;
                format!("{summary_text}\n\n<session_memory>\n{updated}\n</session_memory>")
            }
            Ok(_) => {
                tracing::warn!("SessionMemory: update produced empty content — skipping injection");
                summary_text.to_string()
            }
            Err(e) => {
                tracing::warn!("SessionMemory: update failed ({e}) — skipping injection");
                summary_text.to_string()
            }
        }
    }

    /// Micro-compact parameters derived from `CompactConfig`.
    fn micro_params(&self) -> compact::MicroCompactParams {
        compact::MicroCompactParams {
            keep_recent: 5,
            min_result_tokens: self.compact_config.micro_min_result_tokens,
            head_tokens: self.compact_config.micro_head_tokens,
            tail_tokens: self.compact_config.micro_tail_tokens,
        }
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

    /// Get a reference to the current session state.
    #[must_use]
    pub fn session(&self) -> &SessionState {
        &self.session
    }

    /// Replace the current session state.
    pub fn set_session(&mut self, session: SessionState) {
        self.session = session;
    }

    /// Get the permission checker (shared, for mode changes).
    #[must_use]
    pub fn permission_checker(&self) -> Arc<std::sync::Mutex<PermissionChecker>> {
        self.permission_engine.checker()
    }

    /// The permission engine, to install the per-call pieces a session owns.
    ///
    /// A rule exemption is a property of a *call* — the same Artifact tool
    /// refuses a whole-tool allow rule for a publish and honours it for a read
    /// — and the engine is where the call's arguments are, so the resolver has
    /// to be installed here rather than copied into the checker's table.
    pub fn permission_engine_mut(&mut self) -> &mut PermissionEngine {
        &mut self.permission_engine
    }

    /// Configure the confirmation bus used for interactive permission prompts.
    pub fn set_confirmation_bus(&mut self, bus: ConfirmationBus) {
        self.permission_engine.set_confirmation_bus(bus);
    }

    /// Disable interactive confirmation bus routing.
    pub fn clear_confirmation_bus(&mut self) {
        self.permission_engine.clear_confirmation_bus();
    }

    /// Set the coordinator state for multi-phase workflows.
    pub fn set_coordinator(&mut self, coordinator: crate::coordinator::CoordinatorState) {
        self.coordinator = Some(coordinator);
    }

    /// Set the structured output validator from a JSON schema.
    pub fn set_structured_output(&mut self, schema: serde_json::Value) {
        self.structured_output = Some(StructuredOutputValidator::new(schema));
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
                args: req
                    .arguments
                    .parse()
                    .unwrap_or_else(|_| serde_json::Value::Null),
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
                    response: resp
                        .other_text
                        .clone()
                        .or_else(|| resp.selected.first().cloned()),
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
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &std::collections::HashMap<String, String>,
    ) -> HookFireResult {
        self.hooks.fire_hook(event_name, input, context).await
    }

    /// Run the stop hooks, and enter the state that names them.
    ///
    /// `TurnState::StopHooks` was declared and never entered, and `Stop` was
    /// one of the hook events nothing fired — the same gap seen from two
    /// sides. The state is real now, and the hook fires in it: this is the
    /// last point at which a hook can say something about the turn before it
    /// is closed.
    async fn run_stop_hooks(&self, turn: &mut TurnRuntime, turn_number: u32, stop_reason: &str) {
        turn.transition_to(TurnState::StopHooks);

        let mut ctx = std::collections::HashMap::new();
        ctx.insert("turn_number".to_string(), turn_number.to_string());
        ctx.insert("stop_reason".to_string(), stop_reason.to_string());
        let result = self
            .hooks
            .fire_hook(hook_events::STOP, &format!("turn_{turn_number}"), &ctx)
            .await;
        if !result.errors.is_empty() {
            tracing::warn!("Stop hook errors: {:?}", result.errors);
        }
    }

    /// Fire the TurnEnd hook.
    async fn fire_turn_end_hook(&self, turn_number: u32, stop_reason: &str) -> HookFireResult {
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("turn_number".to_string(), turn_number.to_string());
        ctx.insert("stop_reason".to_string(), stop_reason.to_string());
        self.hooks
            .fire_hook(hook_events::TURN_END, &format!("turn_{turn_number}"), &ctx)
            .await
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
        turn.agent_id = options.agent_id.clone();
        let mut turn_number = self.current_turn + 1;
        let mut last_reason = TerminalReason::Completed;
        let mut provider_resumes: u32 = 0;

        // Add user message
        self.session.messages.push(Message::user(prompt.clone()));

        // Main turn loop
        // A request that arrives after an interrupt is a fresh one.
        self.interrupt.clear();

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
                    last_reason = reason.clone();

                    // An interrupt that arrived while this turn was running —
                    // during a tool, most often — ends the run here rather
                    // than starting another turn.
                    if self.interrupt.is_interrupted() {
                        turn.terminate(TerminalReason::UserInterrupted);
                        last_reason = TerminalReason::UserInterrupted;
                        break;
                    }

                    // Advance coordinator phase if present
                    if let Some(ref mut coordinator) = self.coordinator {
                        let summary = format!("Turn {turn_number}: {reason}");
                        let _ = coordinator.advance(&summary);
                    }

                    let last_is_tool_result = self
                        .session
                        .messages
                        .last()
                        .map(|m| matches!(m.role, crate::types::Role::Tool))
                        .unwrap_or(false);
                    match next_loop_step(&reason, last_is_tool_result, provider_resumes) {
                        LoopStep::Continue => {
                            if matches!(reason, TerminalReason::Resume) {
                                provider_resumes += 1;
                            }
                            turn_number += 1;
                        }
                        LoopStep::Break(reason) => {
                            last_reason = reason;
                            break;
                        }
                    }
                }
                // The error says which kind it is; the loop does not read the
                // message to find out.
                Err(error) => {
                    let reason = error.terminal_reason();
                    turn.terminate(reason.clone());
                    self.current_turn = turn_number;
                    return match error {
                        TurnError::ApiUnavailable { .. } => Ok(reason),
                        TurnError::Fatal { message } => Err(message),
                    };
                }
            }

            if turn.state.is_terminal() {
                if let Some(reason) = turn.terminal_reason.take() {
                    return Ok(reason);
                }
            }
        }

        Ok(turn.terminal_reason.unwrap_or(last_reason))
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
        turn.agent_id = options.agent_id.clone();
        let mut turn_number = self.current_turn + 1;
        let mut last_reason = TerminalReason::Completed;
        let mut provider_resumes: u32 = 0;

        // Add user message
        self.session.messages.push(Message::user(prompt.clone()));

        // Main turn loop
        // A request that arrives after an interrupt is a fresh one.
        self.interrupt.clear();

        loop {
            if options.max_turns > 0 && turn_number > options.max_turns {
                turn.terminate(TerminalReason::MaxTurnsReached);
                break;
            }

            self.cost_tracker.start_turn();
            turn.reset();

            // Execute the turn using streaming
            match self
                .execute_turn_streaming(&mut turn, &options, turn_number)
                .await
            {
                Ok(reason) => {
                    self.current_turn = turn_number;
                    last_reason = reason.clone();

                    // A turn that was interrupted ends here rather than
                    // continuing into the work it was about to do.
                    if self.interrupt.is_interrupted() {
                        turn.terminate(TerminalReason::UserInterrupted);
                        last_reason = TerminalReason::UserInterrupted;
                        break;
                    }

                    // Advance coordinator phase if present
                    if let Some(ref mut coordinator) = self.coordinator {
                        let summary = format!("Turn {turn_number}: {reason}");
                        let _ = coordinator.advance(&summary);
                    }

                    let last_is_tool_result = self
                        .session
                        .messages
                        .last()
                        .map(|m| matches!(m.role, crate::types::Role::Tool))
                        .unwrap_or(false);
                    match next_loop_step(&reason, last_is_tool_result, provider_resumes) {
                        LoopStep::Continue => {
                            if matches!(reason, TerminalReason::Resume) {
                                provider_resumes += 1;
                            }
                            turn_number += 1;
                        }
                        LoopStep::Break(reason) => {
                            last_reason = reason;
                            break;
                        }
                    }
                }
                // The error says which kind it is; the loop does not read the
                // message to find out.
                Err(error) => {
                    let reason = error.terminal_reason();
                    turn.terminate(reason.clone());
                    self.current_turn = turn_number;
                    return match error {
                        TurnError::ApiUnavailable { .. } => Ok(reason),
                        TurnError::Fatal { message } => Err(message),
                    };
                }
            }

            if turn.state.is_terminal() {
                if let Some(reason) = turn.terminal_reason.take() {
                    return Ok(reason);
                }
            }
        }

        Ok(turn.terminal_reason.unwrap_or(last_reason))
    }

    /// Execute a single turn through the state machine.
    async fn execute_turn(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Result<TerminalReason, TurnError> {
        let _start = Instant::now();

        // Phase 1: Initialize → BuildContext
        turn.transition_to(TurnState::BuildContext);
        self.build_context(turn, options);

        // Phase 2: BuildContext → RunPreSubmitHooks
        turn.transition_to(TurnState::RunPreSubmitHooks);

        // Fire UserPromptSubmit hook
        let user_prompt = self.submitted_prompt();
        let ctx = std::collections::HashMap::new();
        let hook_result = self
            .fire_hook(hook_events::USER_PROMPT_SUBMIT, &user_prompt, &ctx)
            .await;
        if hook_result.stop {
            turn.terminate(TerminalReason::Completed);
            return Ok(TerminalReason::Completed);
        }

        // The prompt cache may have gone cold while this session sat idle:
        // the cheapest moment to compact is just before a request that is
        // going to miss the cache anyway.
        if let Some(outcome) = self
            .maybe_compact_on_cache_ttl(turn, options, turn_number)
            .await
        {
            tracing::debug!("cache-TTL compaction: {outcome:?}");
        }

        // The context-size lines have to be checked before the request, not
        // only after an output-token budget trips: by then the input may
        // already be too large to send.
        if let Some(reason) = self
            .maybe_compact_on_threshold(turn, options, turn_number)
            .await
        {
            return Ok(reason);
        }

        // Phase 3: RunPreSubmitHooks → StartRequest
        turn.transition_to(TurnState::StartRequest);

        // Phase 4: StartRequest → StreamAssistant
        turn.transition_to(TurnState::StreamAssistant);
        let api_response = self.send_request(turn, options).await?;
        turn.last_stop_reason = Some(api_response.stop_reason.clone());

        // Record budget and cost
        turn.budget.record_tokens(&api_response.usage);
        self.cost_tracker
            .record_usage(&api_response.usage, &self.model);
        self.last_usage = Some(api_response.usage);

        // Parse the response
        let content = self.validate_structured_output(api_response.content.clone());
        self.push_visible_assistant_message(turn, content.clone());

        // Elicitation heuristic: if the assistant asks a short question, prompt the user
        let mut api_response = api_response;
        if options.handle_elicitation && content.trim().ends_with('?') && content.len() < 200 {
            let result = self
                .handle_elicitation(turn, format!("elic_{turn_number}"), content.clone())
                .await;
            if let ElicitationResult::Responded(resp) = result {
                if let Some(user_text) = resp.other_text.or_else(|| resp.selected.first().cloned())
                {
                    let user_msg = Message::user(user_text);
                    turn.messages.push(user_msg.clone());
                    self.session.messages.push(user_msg);
                    // Re-send request with clarified context
                    api_response = self.send_request(turn, options).await?;
                    turn.budget.record_tokens(&api_response.usage);
                    self.cost_tracker
                        .record_usage(&api_response.usage, &self.model);
                    self.last_usage = Some(api_response.usage);
                    let new_content = self.validate_structured_output(api_response.content.clone());
                    self.push_visible_assistant_message(turn, new_content);
                }
            }
        }

        // Use structured tool calls from the API response (blocking path).
        // Tool calls come from the API response and nowhere else. The model
        // decides what to call; the engine does not re-derive calls from the
        // assistant's prose or from the user's wording.
        let tool_calls = api_response.tool_calls;

        if tool_calls.is_empty() {
            // No tool calls — turn is complete
            turn.transition_to(TurnState::EvaluateContinuation);
            if !turn.budget.should_continue() {
                // Try compact recovery before giving up
                if turn.recovery_count < self.compact_config.max_recovery_retries {
                    turn.transition_to(TurnState::CompactRecovery);
                    let outcome = self
                        .run_compact_recovery(
                            turn,
                            options,
                            turn_number,
                            CompactTrigger::TokenThreshold,
                        )
                        .await;
                    if let Some(reason) = outcome.failure_message() {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                &api_response.stop_reason,
                                &api_response.usage,
                                &[],
                                TerminalReason::Error(reason),
                            )
                            .await);
                    }
                    // Retry the request with compacted context
                    turn.transition_to(TurnState::StartRequest);
                    let api_response = self.send_request(turn, options).await?;
                    turn.budget.record_tokens(&api_response.usage);
                    self.cost_tracker
                        .record_usage(&api_response.usage, &self.model);
                    self.last_usage = Some(api_response.usage);
                    let content = self.validate_structured_output(api_response.content.clone());
                    self.push_visible_assistant_message(turn, content);

                    // After recovery retry, check if we still need to compact
                    if !turn.budget.should_continue() {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                "budget_exhausted",
                                &api_response.usage,
                                &[],
                                TerminalReason::BudgetExhausted,
                            )
                            .await);
                    }

                    if let Some(reason) = self
                        .close_turn(
                            turn,
                            turn_number,
                            &api_response.stop_reason,
                            &api_response.usage,
                            &[],
                        )
                        .await
                    {
                        return Ok(reason);
                    }
                    return Ok(Self::reason_for(&api_response.stop_reason));
                }

                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &api_response.usage,
                        &[],
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }

            if let Some(reason) = self
                .close_turn(
                    turn,
                    turn_number,
                    &api_response.stop_reason,
                    &api_response.usage,
                    &[],
                )
                .await
            {
                return Ok(reason);
            }
            return Ok(Self::reason_for(&api_response.stop_reason));
        }

        // Phase 5: Execute tools
        self.persist_assistant_tool_calls(turn, &tool_calls);
        turn.pending_tool_calls = tool_calls.iter().map(PendingToolCall::from).collect();
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
                let outcome = self
                    .run_compact_recovery(
                        turn,
                        options,
                        turn_number,
                        CompactTrigger::TokenThreshold,
                    )
                    .await;
                if let Some(reason) = outcome.failure_message() {
                    return Ok(self
                        .finish_turn(
                            turn,
                            turn_number,
                            &api_response.stop_reason,
                            &api_response.usage,
                            &pending_for_loop,
                            TerminalReason::Error(reason),
                        )
                        .await);
                }
                // After compact, tool results are still pending — the caller
                // will start the next turn with the compacted context
            } else {
                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &api_response.usage,
                        &pending_for_loop,
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }
        }

        if let Some(reason) = self
            .close_turn(
                turn,
                turn_number,
                &api_response.stop_reason,
                &api_response.usage,
                &pending_for_loop,
            )
            .await
        {
            return Ok(reason);
        }
        // Tool results are pending: the caller starts the next turn.
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
    ) -> Result<TerminalReason, TurnError> {
        let _start = Instant::now();

        // Phase 1: Initialize → BuildContext
        turn.transition_to(TurnState::BuildContext);
        self.build_context(turn, options);

        // Phase 2: BuildContext → RunPreSubmitHooks
        turn.transition_to(TurnState::RunPreSubmitHooks);

        // Fire UserPromptSubmit hook
        let user_prompt = self.submitted_prompt();
        let ctx = std::collections::HashMap::new();
        let hook_result = self
            .fire_hook(hook_events::USER_PROMPT_SUBMIT, &user_prompt, &ctx)
            .await;
        if hook_result.stop {
            turn.terminate(TerminalReason::Completed);
            return Ok(TerminalReason::Completed);
        }

        // The prompt cache may have gone cold while this session sat idle:
        // the cheapest moment to compact is just before a request that is
        // going to miss the cache anyway.
        if let Some(outcome) = self
            .maybe_compact_on_cache_ttl(turn, options, turn_number)
            .await
        {
            tracing::debug!("cache-TTL compaction: {outcome:?}");
        }

        // Check the context lines before sending: an input that is already
        // past the window cannot be fixed by compacting after the fact.
        if let Some(reason) = self
            .maybe_compact_on_threshold(turn, options, turn_number)
            .await
        {
            return Ok(reason);
        }

        // Phase 3: RunPreSubmitHooks → StartRequest → StreamAssistant
        turn.transition_to(TurnState::StartRequest);
        turn.transition_to(TurnState::StreamAssistant);

        // Streaming request
        let StreamedResponse {
            mut content,
            mut tool_calls,
            mut early_calls,
            mut early_results,
            mut usage,
            mut stop_reason,
        } = self.send_request_streaming(turn, options).await?;
        turn.last_stop_reason = Some(stop_reason.clone());

        // Record budget and cost
        turn.budget.record_tokens(&usage);
        self.cost_tracker.record_usage(&usage, &self.model);
        self.last_usage = Some(usage);

        // Validate structured output if a schema is configured
        content = self.validate_structured_output(content);

        // Save assistant message
        self.push_visible_assistant_message(turn, content.clone());

        // Elicitation heuristic: if the assistant asks a short question, prompt the user
        if options.handle_elicitation && content.trim().ends_with('?') && content.len() < 200 {
            let result = self
                .handle_elicitation(turn, format!("elic_{turn_number}"), content.clone())
                .await;
            if let ElicitationResult::Responded(resp) = result {
                if let Some(user_text) = resp.other_text.or_else(|| resp.selected.first().cloned())
                {
                    let user_msg = Message::user(user_text);
                    turn.messages.push(user_msg.clone());
                    self.session.messages.push(user_msg);
                    // Re-send request with clarified context
                    let answered = self.send_request_streaming(turn, options).await?;
                    turn.budget.record_tokens(&answered.usage);
                    self.cost_tracker.record_usage(&answered.usage, &self.model);
                    self.last_usage = Some(answered.usage);
                    content = answered.content;
                    tool_calls = answered.tool_calls;
                    early_calls = answered.early_calls;
                    early_results = answered.early_results;
                    usage = answered.usage;
                    stop_reason = answered.stop_reason;
                    turn.last_stop_reason = Some(stop_reason.clone());
                    self.push_visible_assistant_message(turn, content.clone());
                }
            }
        }

        // Apply early-executed tool results immediately so the model can see them.
        // Their `tool_use` messages go first: a tool_result without the call it
        // answers is not a valid assistant/user pair, and the next request
        // would be rejected by the provider.
        tracing::info!("early_results count: {}", early_results.len());
        self.persist_assistant_tool_calls(turn, &early_calls);
        for result in &early_results {
            let msg = Message::tool(result.clone());
            turn.messages.push(msg.clone());
            self.session.messages.push(msg);
        }
        tracing::info!(
            "session.messages after early_results: {:?}",
            self.session.messages.len()
        );

        // Tool calls are whatever the model emitted during the stream. The
        // engine does not synthesise calls from text or from the user prompt.

        // Loop detection must see every call the model made, including the
        // ones already executed while the stream was open. Recording only the
        // deferred set made a model that repeated an early-executed call look
        // like a model that made no calls at all — and the loop ran forever.
        let pending_for_loop: Vec<PendingToolCall> = early_calls
            .iter()
            .chain(tool_calls.iter())
            .map(PendingToolCall::from)
            .collect();

        if tool_calls.is_empty() {
            // No tool calls — turn complete
            turn.transition_to(TurnState::EvaluateContinuation);
            if !turn.budget.should_continue() {
                if turn.recovery_count < self.compact_config.max_recovery_retries {
                    turn.transition_to(TurnState::CompactRecovery);
                    let outcome = self
                        .run_compact_recovery(
                            turn,
                            options,
                            turn_number,
                            CompactTrigger::TokenThreshold,
                        )
                        .await;
                    if let Some(reason) = outcome.failure_message() {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                &stop_reason,
                                &usage,
                                &pending_for_loop,
                                TerminalReason::Error(reason),
                            )
                            .await);
                    }
                    turn.transition_to(TurnState::StartRequest);
                    let retried = self.send_request_streaming(turn, options).await?;
                    turn.budget.record_tokens(&retried.usage);
                    self.cost_tracker.record_usage(&retried.usage, &self.model);
                    self.last_usage = Some(retried.usage);
                    self.push_visible_assistant_message(turn, retried.content);
                    let usage = retried.usage;

                    if !turn.budget.should_continue() {
                        return Ok(self
                            .finish_turn(
                                turn,
                                turn_number,
                                "budget_exhausted",
                                &usage,
                                &pending_for_loop,
                                TerminalReason::BudgetExhausted,
                            )
                            .await);
                    }

                    let stop_reason = retried.stop_reason.clone();
                    turn.last_stop_reason = Some(stop_reason.clone());
                    if let Some(reason) = self
                        .close_turn(turn, turn_number, &stop_reason, &usage, &pending_for_loop)
                        .await
                    {
                        return Ok(reason);
                    }
                    return Ok(Self::reason_for(&stop_reason));
                }
                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &usage,
                        &pending_for_loop,
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }
            if let Some(reason) = self
                .close_turn(turn, turn_number, &stop_reason, &usage, &pending_for_loop)
                .await
            {
                return Ok(reason);
            }
            return Ok(Self::reason_for(&stop_reason));
        }

        // Phase 5: Execute tools
        self.persist_assistant_tool_calls(turn, &tool_calls);
        turn.pending_tool_calls = tool_calls.iter().map(PendingToolCall::from).collect();

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
                let outcome = self
                    .run_compact_recovery(
                        turn,
                        options,
                        turn_number,
                        CompactTrigger::TokenThreshold,
                    )
                    .await;
                if let Some(reason) = outcome.failure_message() {
                    return Ok(self
                        .finish_turn(
                            turn,
                            turn_number,
                            &stop_reason,
                            &usage,
                            &pending_for_loop,
                            TerminalReason::Error(reason),
                        )
                        .await);
                }
            } else {
                return Ok(self
                    .finish_turn(
                        turn,
                        turn_number,
                        "budget_exhausted",
                        &usage,
                        &pending_for_loop,
                        TerminalReason::BudgetExhausted,
                    )
                    .await);
            }
        }

        if let Some(reason) = self
            .close_turn(turn, turn_number, &stop_reason, &usage, &pending_for_loop)
            .await
        {
            return Ok(reason);
        }
        // Tool results are pending: the caller starts the next turn.
        Ok(TerminalReason::Completed)
    }

    /// Close a turn: stop hooks, finalize, cost, turn-end hook, loop check.
    ///
    /// Both turn paths end here, and they had drifted apart: the streaming one
    /// passed a hardcoded `"end_turn"` as the reason and never reported a
    /// truncated answer as [`TerminalReason::TokenLimitStop`], so a response
    /// the model ran out of room for looked exactly like a finished turn.
    /// Returns a reason when the loop detector has had enough.
    async fn close_turn(
        &mut self,
        turn: &mut TurnRuntime,
        turn_number: u32,
        stop_reason: &str,
        usage: &UsageSummary,
        pending_for_loop: &[PendingToolCall],
    ) -> Option<TerminalReason> {
        self.run_stop_hooks(turn, turn_number, stop_reason).await;
        turn.transition_to(TurnState::Finalize);
        self.cost_tracker.end_turn(turn_number, usage, &self.model);
        let _ = self.fire_turn_end_hook(turn_number, stop_reason).await;

        // A provider-paused turn is not an empty turn. The model answered and
        // the provider asked for the same request again; recording that as a
        // tool-call-free turn would let the loop detector call a legitimate
        // pause a repetition and end the run after three of them.
        if matches!(classify_stop_reason(stop_reason), StopDisposition::Resume) {
            return None;
        }

        self.record_and_check_loop(pending_for_loop)
    }

    /// Close a turn that ends for a reason other than how the model stopped.
    ///
    /// Budget exhaustion and recovery failure used to return straight out of
    /// the turn loop — `turn.terminate(...)` and a `return` — without running
    /// the Stop/TurnEnd hooks, the cost bookkeeping, or the loop check every
    /// model-ended turn goes through. They now take the same closing path and
    /// terminate afterwards.
    ///
    /// A loop the detector reports takes precedence over the reason passed in:
    /// it is the more specific diagnosis of why the run must end.
    async fn finish_turn(
        &mut self,
        turn: &mut TurnRuntime,
        turn_number: u32,
        stop_reason: &str,
        usage: &UsageSummary,
        pending_for_loop: &[PendingToolCall],
        reason: TerminalReason,
    ) -> TerminalReason {
        let detected = self
            .close_turn(turn, turn_number, stop_reason, usage, pending_for_loop)
            .await;
        let reason = detected.unwrap_or(reason);
        turn.terminate(reason.clone());
        reason
    }

    /// The reason a closed turn reports, given how the model stopped.
    ///
    /// A paused turn reports [`TerminalReason::Resume`] rather than
    /// `Completed`: both are "not an error", but only one of them tells the
    /// loop to send the request again.
    #[must_use]
    fn reason_for(stop_reason: &str) -> TerminalReason {
        if stop_reason_eq(stop_reason, "max_tokens") {
            TerminalReason::TokenLimitStop
        } else if matches!(classify_stop_reason(stop_reason), StopDisposition::Resume) {
            TerminalReason::Resume
        } else {
            TerminalReason::Completed
        }
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
                tracing::warn!("Structured output validation failed: {error} (retry {retries})");
                let warning = format!("\n\n<!-- Structured output validation failed: {error} -->");
                format!("{content}{warning}")
            }
        }
    }

    fn push_visible_assistant_message(&mut self, turn: &mut TurnRuntime, content: String) {
        if content.trim().is_empty() {
            return;
        }
        let assistant_msg = Message::assistant(content);
        turn.messages.push(assistant_msg.clone());
        self.session.messages.push(assistant_msg);
    }

    /// Build the prompt context from session state.
    /// The prompt this turn is answering.
    ///
    /// `execute_turn` takes no prompt parameter — the prompt is the last user
    /// message on the session — and the `UserPromptSubmit` hook used to be
    /// handed `custom_system_prompt` instead. That is almost always empty, so a
    /// hook that inspects the prompt it is supposedly being told about saw
    /// nothing at all.
    fn submitted_prompt(&self) -> String {
        self.session
            .messages
            .iter()
            .rev()
            .find_map(|message| match (&message.role, &message.content) {
                (crate::types::Role::User, crate::types::MessageContent::Text(text)) => {
                    Some(text.clone())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    fn build_context(&mut self, turn: &mut TurnRuntime, options: &SubmitOptions) {
        // Assemble context using PromptCompiler
        let available_tools = self.tools.list_tools();
        let ctx = PromptContext {
            cwd: self.session.cwd.as_path(),
            available_tools,
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

        // Inject JSON schema instructions for structured output
        if let Some(ref schema) = options.json_schema {
            let instruction = format!(
                "You must respond with ONLY valid JSON that matches this schema:\n{}",
                serde_json::to_string_pretty(schema).unwrap_or_default()
            );
            turn.messages.push(Message::system(instruction));
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

    /// Summarize a conversation interval via the attached compaction
    /// backend (local/remote dual-track seam).
    ///
    /// With no backend attached this is exactly the built-in local LLM
    /// summary. With a backend attached, a backend failure or empty summary
    /// falls back to the built-in local path — the backend is an
    /// optimization, never a new failure mode. Both paths collapse to
    /// `Result<String, String>` so the PTL retry and rule-based fallback
    /// below treat them uniformly. No remote compaction implementation
    /// exists yet; a backend is opt-in via `set_compaction_backend` and
    /// absent by default.
    async fn summarize_for_compaction(
        &self,
        conversation: &str,
        max_tokens: u32,
    ) -> Result<String, String> {
        let local = || async {
            crate::prompts::generate_compact_summary(
                self.api_client.as_ref(),
                conversation,
                None,
                max_tokens,
            )
            .await
            .map_err(|e| e.to_string())
        };
        let Some(backend) = self.compaction_backend.as_ref() else {
            return local().await;
        };
        tracing::info!(
            "CompactRecovery: summarizing via compaction backend '{}'",
            backend.name()
        );
        match backend
            .summarize(crate::compaction_backend::CompactionRequest {
                conversation: conversation.to_string(),
                focus: None,
                max_output_tokens: max_tokens,
            })
            .await
        {
            Ok(text) if !text.trim().is_empty() => Ok(text),
            Ok(_) => {
                tracing::warn!(
                    "CompactionBackend '{}': empty summary — falling back to local summary",
                    backend.name()
                );
                local().await
            }
            Err(e) => {
                tracing::warn!(
                    "CompactionBackend '{}': failed ({e}) — falling back to local summary",
                    backend.name()
                );
                local().await
            }
        }
    }

    /// Run compact recovery when context is too large.
    ///
    /// Returns a named [`CompactOutcome`] for every path, including the ones
    /// that do nothing: "why compaction did not happen" is a question a caller
    /// and a test both need answered, and a log line answers it for neither.
    #[allow(clippy::unused_async)]
    async fn run_compact_recovery(
        &self,
        turn: &mut TurnRuntime,
        _options: &SubmitOptions,
        _turn_number: u32,
        trigger: CompactTrigger,
    ) -> CompactOutcome {
        // Estimate current token count
        let estimated_tokens = PromptCompiler::estimate_message_tokens(&turn.messages);

        // The strategy follows the trigger. A cold cache is a moment to tidy
        // up cheaply, not a reason to rewrite history: it takes the lightest
        // pass and never calls the summarizer. Crossing the token line takes
        // the strategy the thresholds name.
        // `Reactive` is the strategy for a response the model ran out of room
        // for. It has to see the actual stop reason, not a placeholder.
        let stop_reason = turn.last_stop_reason.as_deref().unwrap_or("end_turn");
        let strategy = match trigger {
            CompactTrigger::CacheTtl => CompactStrategy::Micro,
            CompactTrigger::TokenThreshold => match &self.context_window_resolver {
                Some(resolver) => compact::select_compact_strategy_with(
                    stop_reason,
                    estimated_tokens,
                    &self.model,
                    &self.compact_config,
                    resolver,
                ),
                None => compact::select_compact_strategy(
                    stop_reason,
                    estimated_tokens,
                    &self.model,
                    &self.compact_config,
                ),
            },
        };

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
                return CompactOutcome::StopHookTerminated { reason };
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
        ctx.insert("trigger".to_string(), format!("{trigger:?}"));
        let pre_compact_result = self
            .fire_hook(hook_events::PRE_COMPACT, "compact_recovery", &ctx)
            .await;
        if !pre_compact_result.errors.is_empty() {
            tracing::warn!("PreCompact hook errors: {:?}", pre_compact_result.errors);
        }

        // Circuit breaker: stop auto-compacting against an irrecoverable
        // context (e.g., context_window exceeded by a single message).
        if turn.compact_failures >= self.compact_config.max_consecutive_failures {
            return CompactOutcome::CircuitBreakerOpen {
                consecutive_failures: turn.compact_failures,
                max: self.compact_config.max_consecutive_failures,
            };
        }

        // ── LLM summary compaction (preferred path) ──
        // Preserve head (first user message) and tail (recent turns) verbatim;
        // LLM-summarize the interval in between. Falls back to rule-based
        // strategies on any failure.
        //
        // The TTL trigger skips this: a summary is a model call, and a
        // proactive tidy-up is not worth one — and the lightest rule-based
        // pass is the whole point of compacting while the cache is cold.
        let tokens_before = estimated_tokens;
        let split = compact::summary_split(&turn.messages, 3);
        let summary_input = compact::build_summary_input(
            &turn.messages,
            &split,
            self.compact_config.summary_input_max_tokens,
        );

        let mut summary_failure: Option<SummaryOutcome> = None;
        if trigger == CompactTrigger::TokenThreshold {
            if summary_input.len() >= 2 {
                // PTL resilience: if the summary request fails (e.g., the input
                // itself is too long), retry once with half the input budget —
                // `build_summary_input` re-caps by dropping the oldest messages.
                let mut summary_result = self
                    .summarize_for_compaction(
                        &summary_input.join("\n\n"),
                        self.compact_config.summary_output_max_tokens,
                    )
                    .await;
                if summary_result.is_err() {
                    tracing::warn!(
                        "CompactRecovery: summary failed — retrying with half input budget"
                    );
                    let halved = compact::build_summary_input(
                        &turn.messages,
                        &split,
                        self.compact_config.summary_input_max_tokens / 2,
                    );
                    if halved.len() >= 2 {
                        summary_result = self
                            .summarize_for_compaction(
                                &halved.join("\n\n"),
                                self.compact_config.summary_output_max_tokens,
                            )
                            .await;
                    }
                }
                match summary_result {
                    Ok(summary_text) if !summary_text.trim().is_empty() => {
                        // Refresh the persistent session memory from the same
                        // summarized interval and merge it into the summary.
                        // Box::pin breaks the future-size cycle through the
                        // nested background-feature call chain.
                        let summary_text =
                            Box::pin(self.refresh_session_memory(&summary_text, &summary_input))
                                .await;
                        let mut result = compact::assemble_summary_result(
                            &turn.messages,
                            &split,
                            &summary_text,
                            tokens_before,
                            0,
                        );
                        // Post-compact restore: breadcrumb the file paths whose
                        // contents were compacted away so the agent can re-read.
                        let restored_paths =
                            compact::extract_recent_file_paths(&turn.messages, &split, 5);
                        if let Some(restore_msg) = compact::file_restore_message(&restored_paths) {
                            result.messages.push(restore_msg);
                        }
                        turn.compact_failures = 0;
                        return self
                            .finish_compaction(turn, result, trigger, SummaryOutcome::Summarized)
                            .await;
                    }
                    Ok(_) => {
                        summary_failure = Some(SummaryOutcome::Empty);
                        tracing::warn!(
                            "CompactRecovery: LLM summary empty — falling back to rules"
                        );
                    }
                    Err(e) => {
                        summary_failure = Some(SummaryOutcome::Failed { error: e.clone() });
                        tracing::warn!(
                            "CompactRecovery: LLM summary failed ({e}) — falling back to rules"
                        );
                    }
                }
            } else {
                tracing::debug!("CompactRecovery: summarize range too small — rule-based path");
                summary_failure = Some(SummaryOutcome::RangeTooSmall {
                    messages: summary_input.len(),
                });
            }
        }

        // ── Rule-based fallback (existing strategies) ──
        // The rule-based strategies take a target. Use the resolved line for
        // this model, not the configured fallback, which is sized for a window
        // that may not be this one.
        let thresholds = self.compact_thresholds();
        let result: CompactResult = match strategy {
            CompactStrategy::Micro => compact::micro_compact(&turn.messages, self.micro_params()),
            CompactStrategy::Auto => compact::auto_compact(&turn.messages, thresholds.auto),
            CompactStrategy::Reactive => compact::reactive_compact(&turn.messages, 3),
            CompactStrategy::Snip => compact::snip_compact(
                &turn.messages,
                self.compact_config.snip_target_tokens.min(thresholds.auto),
            ),
        };

        let summary = summary_failure.unwrap_or(SummaryOutcome::NotRequested);
        self.finish_compaction(turn, result, trigger, summary).await
    }

    /// Shared tail of every compaction path: tokens bookkeeping, boundary
    /// event, transcript persistence, retry counters, and the PostCompact hook.
    ///
    /// The compaction-failure circuit breaker is driven here: an attempt that
    /// fails to shrink the context counts as a failure and resets on progress,
    /// so `run_compact_recovery` stops retrying once the context is
    /// irrecoverable.
    ///
    /// Only a threshold-triggered compaction spends a recovery retry. A
    /// cache-TTL pass is maintenance, and charging it to the recovery budget
    /// would let tidying up starve a real recovery later.
    async fn finish_compaction(
        &self,
        turn: &mut TurnRuntime,
        mut result: CompactResult,
        trigger: CompactTrigger,
        summary: SummaryOutcome,
    ) -> CompactOutcome {
        // Recompute tokens removed from the actual post-compaction messages.
        let tokens_after = PromptCompiler::estimate_message_tokens(&result.messages);
        let tokens_before = PromptCompiler::estimate_message_tokens(&turn.messages);
        result.tokens_removed = tokens_before.saturating_sub(tokens_after);

        // Circuit breaker accounting: no progress counts as a failure.
        let made_progress = tokens_after < tokens_before;
        if made_progress {
            turn.compact_failures = 0;
        } else {
            turn.compact_failures = turn.compact_failures.saturating_add(1);
        }

        // Check if compact boundary was inserted
        if result.boundary_inserted {
            // Emit compact boundary event
            let _ = self.event_tx.send(EngineEvent::CompactBoundary);
            tracing::debug!("CompactRecovery: boundary marker inserted");
        }

        // Update turn messages with compacted messages
        let messages_remaining = result.messages.len();
        let strategy = result.strategy;
        turn.messages = result.messages;

        // Persist compact to transcript file on disk
        if let Some(ref base_path) = self.transcript_path {
            let session_id_str = self.session.session_id.to_string();
            let transcript_file = base_path.join(format!("{session_id_str}.jsonl"));

            // Back up first: compaction overwrites the transcript, and the
            // messages it drops are gone from the live file with no undo.
            match Self::write_transcript_backup(&transcript_file) {
                Ok(Some(path)) => {
                    tracing::info!("PreCompact: transcript backed up to {}", path.display());
                }
                Ok(None) => { /* nothing on disk yet — nothing to lose */ }
                Err(e) => tracing::warn!("PreCompact: could not back up transcript: {e}"),
            }

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

        // Increment recovery count (threshold-triggered compaction only).
        if trigger == CompactTrigger::TokenThreshold {
            turn.recovery_count += 1;
        }

        // Fire PostCompact hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert(
            "tokens_removed".to_string(),
            result.tokens_removed.to_string(),
        );
        ctx.insert(
            "messages_remaining".to_string(),
            messages_remaining.to_string(),
        );
        ctx.insert("strategy".to_string(), format!("{strategy:?}"));
        ctx.insert("trigger".to_string(), format!("{trigger:?}"));
        ctx.insert("summary".to_string(), format!("{summary:?}"));
        let post_compact_result = self
            .fire_hook(hook_events::POST_COMPACT, "compact_complete", &ctx)
            .await;
        if !post_compact_result.errors.is_empty() {
            tracing::warn!("PostCompact hook errors: {:?}", post_compact_result.errors);
        }

        let outcome = if made_progress {
            CompactOutcome::Compacted {
                strategy,
                trigger,
                tokens_removed: result.tokens_removed,
                messages_remaining,
                summary,
            }
        } else {
            CompactOutcome::NoProgress {
                tokens_before,
                tokens_after,
                summary,
            }
        };
        tracing::debug!(
            "CompactRecovery: {outcome:?} ({} tokens removed, {} messages remaining)",
            result.tokens_removed,
            turn.messages.len(),
        );
        outcome
    }

    /// Copy the transcript aside before compaction overwrites it.
    ///
    /// Returns the path written, or `None` when there is no transcript on disk
    /// yet. The name is timestamped so a second compaction does not overwrite
    /// the first backup with an already-compacted transcript — the point of a
    /// backup is to keep what the *first* one had.
    fn write_transcript_backup(path: &PathBuf) -> std::io::Result<Option<PathBuf>> {
        if !path.exists() {
            return Ok(None);
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("transcript.jsonl");
        let backup_path = path.with_file_name(format!("{name}.backup.{stamp}"));
        std::fs::copy(path, &backup_path)?;
        Ok(Some(backup_path))
    }

    /// Whether the prompt cache has gone cold since the last request.
    fn cache_ttl_elapsed(&self) -> bool {
        let Some(last) = self.last_request_at else {
            return false;
        };
        compact::should_compact_on_cache_ttl(
            last.elapsed(),
            std::time::Duration::from_secs(self.compact_config.cache_ttl_secs),
            std::time::Duration::from_secs(self.compact_config.cache_ttl_early_exit_secs),
        )
    }

    /// Compact proactively when the prompt cache has gone cold.
    ///
    /// Every request refreshes the cache, so a session that sat idle past the
    /// TTL pays a cache miss on its next request whatever happens — which is
    /// the cheapest moment to rewrite the context. This runs *before* the
    /// request, and only while the context is below the token line: at or
    /// above it the threshold path owns compaction, and running both would
    /// compact twice for one reason.
    ///
    /// The reference implementation schedules this on a timer. The engine has
    /// no background task, so the check happens at the turn boundary: a
    /// session that never sends another request never compacts. That is the
    /// honest limit of doing this inside the turn loop.
    ///
    /// Compact before a request when the estimated context has crossed a line.
    ///
    /// `CompactThresholds` answers "how full is the window"; until this call
    /// existed, only the output-token budget could trigger compaction, so a
    /// large input could overflow a small-window model before anything ran.
    ///
    /// Returns `Some(reason)` only when the context is still blocked after a
    /// compaction attempt; at the compact line the turn continues.
    async fn maybe_compact_on_threshold(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Option<TerminalReason> {
        let estimated = PromptCompiler::estimate_message_tokens(&turn.messages);
        let thresholds = self.compact_thresholds();
        let level = thresholds.level(estimated);

        match level {
            compact::ContextLevel::Ok => return None,
            compact::ContextLevel::Warn => {
                tracing::warn!(
                    estimated_tokens = estimated,
                    warn_at = thresholds.warn,
                    "context is approaching the compaction line"
                );
                return None;
            }
            compact::ContextLevel::Compact | compact::ContextLevel::Blocked => {}
        }

        if turn.recovery_count >= self.compact_config.max_recovery_retries {
            return if level == compact::ContextLevel::Blocked {
                Some(TerminalReason::Error(format!(
                    "context window is full ({estimated} estimated tokens) and the compaction \
                     retry budget is spent"
                )))
            } else {
                None
            };
        }

        let outcome = self
            .run_compact_recovery(turn, options, turn_number, CompactTrigger::TokenThreshold)
            .await;
        if let Some(reason) = outcome.failure_message() {
            return Some(TerminalReason::Error(reason));
        }

        if level == compact::ContextLevel::Blocked {
            let after = PromptCompiler::estimate_message_tokens(&turn.messages);
            if thresholds.level(after) == compact::ContextLevel::Blocked {
                return Some(TerminalReason::Error(format!(
                    "context window is still full after compaction ({after} estimated tokens)"
                )));
            }
        }

        None
    }

    /// On the compacting path the turn is left in `CompactRecovery` — the
    /// caller's next move to `StartRequest` is the one the graph already
    /// allows from there.
    async fn maybe_compact_on_cache_ttl(
        &self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
        turn_number: u32,
    ) -> Option<CompactOutcome> {
        if !self.cache_ttl_elapsed() {
            return None;
        }

        let estimated_tokens = PromptCompiler::estimate_message_tokens(&turn.messages);
        let thresholds = self.compact_thresholds();
        if matches!(
            thresholds.level(estimated_tokens),
            compact::ContextLevel::Compact | compact::ContextLevel::Blocked
        ) {
            return Some(CompactOutcome::DeferredToThresholdPath { estimated_tokens });
        }

        if turn.recovery_count >= self.compact_config.max_recovery_retries {
            return Some(CompactOutcome::SkippedRecoveryBudget {
                recovery_count: turn.recovery_count,
                max: self.compact_config.max_recovery_retries,
            });
        }

        turn.transition_to(TurnState::CompactRecovery);
        let outcome = self
            .run_compact_recovery(turn, options, turn_number, CompactTrigger::CacheTtl)
            .await;
        // The caller moves on to StartRequest; the graph allows it from here.
        Some(outcome)
    }

    /// Write a full replacement to the transcript file with the given messages.
    ///
    /// Uses a temp file + atomic rename for crash safety.
    fn write_transcript_replacement(path: &PathBuf, messages: &[Message]) -> std::io::Result<()> {
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
        options: &SubmitOptions,
    ) -> Result<ApiResponse, TurnError> {
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

        let messages = Self::provider_messages(&turn.messages);

        let tools = self.tools.list_tools();
        let request_client = self
            .api_client
            .with_request_context(self.build_request_context(self.current_turn, options));
        // Mark the request time: a successful response refreshes the provider's
        // prompt cache, and the TTL trigger measures idleness from here.
        self.last_request_at = Some(Instant::now());
        let response = match request_client
            .create_message(
                &system,
                &messages,
                &self.model,
                self.max_tokens,
                &tools,
            )
            .await
        {
            Ok(resp) => resp,
            Err(crate::api_client::ApiError::RetryExhausted {
                attempts,
                last_error_message: _,
            }) => {
                let message = format!(
                    "The AI service is temporarily unavailable after {attempts} attempts. Please try again later."
                );
                let _ = self.event_tx.send(EngineEvent::Notification {
                    message: message.clone(),
                });
                return Err(TurnError::ApiUnavailable { message });
            }
            Err(e) => {
                // A transport failure that ran out of retries is the same
                // question — try later — and is classified by kind rather than
                // by reading the message.
                return Err(if crate::api_client::is_retryable(&e) {
                    TurnError::ApiUnavailable {
                        message: e.to_string(),
                    }
                } else {
                    TurnError::Fatal {
                        message: e.to_string(),
                    }
                });
            }
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
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
    ) -> Result<StreamedResponse, TurnError> {
        use futures::StreamExt;
        use std::collections::HashSet;

        /// Maximum total attempts for a streaming request:
        /// 1 initial + `MAX_STREAM_ATTEMPTS - 1` auto-retries.
        const MAX_STREAM_ATTEMPTS: u32 = 3;

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

        let messages = Self::provider_messages(&turn.messages);
        let tools = self.tools.list_tools();

        // Text survives a retry. A stream cannot be resumed at an offset —
        // the retry re-requests the turn — but what the first attempt already
        // delivered is kept and merged with what the retry produces, so an
        // interruption costs the reader nothing that had already arrived.
        // `StreamTextMerger` emits only the growth, once per delta.
        let mut stream_text = StreamTextMerger::new();

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            // A retry regenerates the answer from the top; start comparing
            // again from the first character.
            stream_text.begin_attempt();
            let request_client = self
                .api_client
                .with_request_context(self.build_request_context(self.current_turn, options));
            let mut stream = request_client
                .create_message_streaming(
                    &system,
                    &messages,
                    &self.model,
                    self.max_tokens,
                    &tools,
                )
                .await;
            self.last_request_at = Some(Instant::now());

            // Per-attempt state. Text is merged into `stream_text` as it
            // arrives; tool calls are the attempt's own — a retry re-asks the
            // model, and executing a call twice because two attempts both
            // proposed it would double its side effects.
            let mut assembler = StreamingToolCallAssembler::new();
            let mut usage = UsageSummary {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            let mut early_calls: Vec<ToolCall> = Vec::new();
            let mut early_results: Vec<ToolResult> = Vec::new();
            let mut deferred_calls: Vec<ToolCall> = Vec::new();
            let mut stream_failed = false;
            let mut stop_reason: Option<String> = None;
            let mut retry_requested = false;

            while let Some(event) = stream.next().await {
                // Stop reading a stream the person has asked to abandon: the
                // rest of it is tokens nobody is waiting for.
                if self.interrupt.is_interrupted() {
                    stream_failed = true;
                    break;
                }
                match event.clone() {
                    StreamEvent::TextDelta(ref delta) => {
                        if let Some(new_text) = stream_text.push_delta(delta) {
                            let _ = self.event_tx.send(EngineEvent::TextDelta(new_text));
                        }
                    }
                    StreamEvent::ToolCallBlockStart { .. } => {}
                    StreamEvent::ToolCallBlockEnd { .. } => {
                        // Completion handled below via assembler.process_event
                    }
                    StreamEvent::ToolCallInputDelta { ref id, ref delta } => {
                        tracing::trace!("tool call input delta for {id}: {} bytes", delta.len());
                    }
                    StreamEvent::UsageSummary(ref u) => {
                        usage = u.clone();
                    }
                    StreamEvent::StopReason(ref r) => {
                        stop_reason = Some(r.clone());
                    }
                    StreamEvent::StreamEnd => {
                        // Finalize pending tool calls — handled after loop
                    }
                    StreamEvent::Error(ref e) => {
                        // A retry re-asks the model and re-runs what it
                        // proposes. If this attempt already executed a tool,
                        // retrying can repeat its side effects, so the partial
                        // result is returned instead.
                        if early_calls.is_empty()
                            && attempt < MAX_STREAM_ATTEMPTS
                            && crate::api_client::is_retryable(e)
                        {
                            tracing::warn!(
                                "streaming network error on attempt {attempt} ({} bytes received): {e} — re-requesting",
                                stream_text.delivered().len()
                            );
                            let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                                reason: "error".into(),
                                partial_len: stream_text.delivered().len(),
                                will_retry: true,
                            });
                            tokio::time::sleep(std::time::Duration::from_millis(
                                500u64 << (attempt - 1),
                            ))
                            .await;
                            retry_requested = true;
                            // Text carries into the retry; the attempt's tool
                            // calls do not (see the note above the loop).
                            break;
                        }
                        tracing::warn!(
                            "streaming network error ({} bytes received so far): {e}",
                            stream_text.delivered().len()
                        );
                        stream_failed = true;
                        // Fail-open: stop consuming the stream but return
                        // whatever partial text and tool calls we have.
                        break;
                    }
                }

                // Process event through assembler and execute completed tool
                // calls immediately while the stream is still running.
                for call in assembler.process_event(event) {
                    let recorded = call.clone();
                    match self.execute_early_tool_call(turn, call, options).await {
                        Ok(Some(result)) => {
                            early_calls.push(recorded);
                            early_results.push(result);
                        }
                        Ok(None) => { /* deferred — call already tracked */ }
                        Err(e) => {
                            // An error here is not a reason to drop the call:
                            // the model still has to see a result for the
                            // tool_use, or the history is an invalid pair.
                            tracing::warn!("early tool execution failed: {e}");
                            let id = recorded.id.clone();
                            early_calls.push(recorded);
                            early_results.push(ToolResult {
                                tool_call_id: id,
                                output: format!("tool execution failed: {e}"),
                                is_error: true,
                            });
                        }
                    }
                }
            }

            if !retry_requested {
                // Stream ended — finalize any remaining pending calls.
                for call in assembler.process_event(StreamEvent::StreamEnd) {
                    let recorded = call.clone();
                    match self.execute_early_tool_call(turn, call, options).await {
                        Ok(Some(result)) => {
                            early_calls.push(recorded);
                            early_results.push(result);
                        }
                        Ok(None) => { /* deferred */ }
                        Err(e) => {
                            tracing::warn!("early tool execution failed: {e}");
                            let id = recorded.id.clone();
                            early_calls.push(recorded);
                            early_results.push(ToolResult {
                                tool_call_id: id,
                                output: format!("tool execution failed: {e}"),
                                is_error: true,
                            });
                        }
                    }
                }
            }

            // A retryable mid-stream error with retry attempts left:
            // restart the request from scratch.
            if retry_requested {
                continue;
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

            if stream_failed
                && stream_text.delivered().is_empty()
                && early_results.is_empty()
                && deferred_calls.is_empty()
            {
                tracing::info!(
                    "streaming request yielded no usable events; retrying same turn non-streaming"
                );
                if let Ok(resp) = self.send_request(turn, options).await {
                    // The fallback keeps the response's own stop reason: a
                    // truncated answer is still truncated when it arrives this
                    // way.
                    return Ok(StreamedResponse {
                        content: resp.content,
                        tool_calls: resp.tool_calls,
                        early_calls: Vec::new(),
                        early_results: Vec::new(),
                        usage: resp.usage,
                        stop_reason: resp.stop_reason,
                    });
                }
            }

            // An interrupt is not a failure: nothing is wrong, the person
            // asked for this to stop, and saying "error" would send whoever
            // reads the event looking for a fault.
            if self.interrupt.is_interrupted() {
                let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                    reason: "user".into(),
                    partial_len: stream_text.delivered().len(),
                    will_retry: false,
                });
                // Whatever arrived before the interrupt is kept: a partial
                // answer is what the person asked to stop at, and throwing it
                // away would lose the tokens they already paid for.
                return Ok(StreamedResponse {
                    content: stream_text.into_delivered(),
                    tool_calls: deferred_calls,
                    early_calls,
                    early_results,
                    usage,
                    stop_reason: stop_reason.unwrap_or_else(|| "interrupted".to_string()),
                });
            }

            if stream_failed {
                let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                    reason: "error".into(),
                    partial_len: stream_text.delivered().len(),
                    will_retry: false,
                });
            } else if matches!(stop_reason.as_deref(), Some("max_tokens") | Some("length")) {
                // Legitimate provider-completed truncation: not an error and
                // not retryable (re-requesting would loop and double cost).
                let _ = self.event_tx.send(EngineEvent::StreamInterrupted {
                    reason: stop_reason.clone().unwrap_or_default(),
                    partial_len: stream_text.delivered().len(),
                    will_retry: false,
                });
            }

            return Ok(StreamedResponse {
                content: stream_text.into_delivered(),
                tool_calls: deferred_calls,
                early_calls,
                early_results,
                usage,
                stop_reason: stop_reason.unwrap_or_else(|| "end_turn".to_string()),
            });
        }
    }

    /// Execute a single tool call immediately during streaming.
    ///
    /// Runs the full validation pipeline (allowed_tools_override, PreToolUse
    /// hook, PermissionEngine) and executes the tool if allowed.
    ///
    /// - `Ok(Some(result))` — the tool was executed, denied, or resolved after
    ///   user interaction.
    /// - `Ok(None)` — reserved for a deferred call with no immediate result.
    async fn execute_early_tool_call(
        &mut self,
        turn: &mut TurnRuntime,
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
        let pre_hook = self
            .hooks
            .fire_hook(hook_events::PRE_TOOL_USE, &call.name, &ctx)
            .await;
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

        let _ = self.event_tx.send(EngineEvent::ToolCallStart {
            id: call.id.clone(),
            name: call.name.clone(),
            summary: call.activity_summary(),
        });

        // 3. Permission check
        let (_request, perm_result) = self
            .permission_engine
            .evaluate(
                &call.name,
                call.input.to_string(),
                None,
                crate::tool_scheduler::permission_context_for(turn.agent_id.as_deref(), &call.id),
            )
            .await
            .map_err(|e| format!("permission check failed: {e}"))?;

        match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                let outcome = self.tools.execute(&call.id, &call.name, &call.input).await;
                let result = outcome.result;

                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert(
                    "tool_output_len".to_string(),
                    result.output.len().to_string(),
                );
                let post_result = self
                    .hooks
                    .fire_hook(hook_events::POST_TOOL_USE, &call.name, &ctx)
                    .await;
                if !post_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUse hook errors for {}: {:?}",
                        call.name,
                        post_result.errors
                    );
                }

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
                    output: crate::permissions::PermissionResult::unapproved_tool_message(
                        &call.name, &reason,
                    ),
                    is_error: true,
                };
                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let fail_result = self
                    .hooks
                    .fire_hook(hook_events::POST_TOOL_USE_FAILURE, &call.name, &ctx)
                    .await;
                if !fail_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUseFailure hook errors for {}: {:?}",
                        call.name,
                        fail_result.errors
                    );
                }
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
                // The reason is the only thing that tells the model what to
                // do differently.
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' denied: {reason}", call.name),
                    is_error: true,
                };
                let mut ctx = std::collections::HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let fail_result = self
                    .hooks
                    .fire_hook(hook_events::POST_TOOL_USE_FAILURE, &call.name, &ctx)
                    .await;
                if !fail_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUseFailure hook errors for {}: {:?}",
                        call.name,
                        fail_result.errors
                    );
                }
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
                turn.pending_elevation = Some(PendingElevation {
                    tool_call_id: call.id,
                    tool_name: call.name,
                    input: call.input,
                    request_id: elevation_id,
                    reason,
                });
                self.resolve_pending_elevation(turn).await
            }
        }
    }

    /// Execute pending tool calls with permission checks.
    async fn execute_tool_calls(
        &mut self,
        turn: &mut TurnRuntime,
        options: &SubmitOptions,
    ) -> Result<Vec<ToolResult>, String> {
        // An interrupt that arrived while the model was answering stops the
        // tools as well: the person asked for this to end, not for one more
        // batch of edits to land.
        if self.interrupt.is_interrupted() {
            return Ok(turn
                .pending_tool_calls
                .iter()
                .map(|call| ToolResult {
                    tool_call_id: call.id.clone(),
                    output: "the turn was interrupted".to_string(),
                    is_error: true,
                })
                .collect());
        }

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

        let mut results = outcome.results;
        for elevation in outcome.pending_elevations {
            let tool_call_id = elevation.tool_call_id.clone();
            turn.pending_elevation = Some(elevation);
            if let Some(result) = self.resolve_pending_elevation(turn).await? {
                if let Some(slot) = results
                    .iter_mut()
                    .find(|result| result.tool_call_id == tool_call_id)
                {
                    *slot = result;
                }
            }
        }
        turn.pending_tool_calls.clear();
        Ok(results)
    }

    /// Ask the interactive frontend for an elevation decision, then resolve it.
    ///
    /// The confirmation bus is the same channel used by ordinary permission
    /// prompts. Keeping the wait inside the engine is important: `submit` and
    /// `submit_streaming` own the [`TurnRuntime`], so an external UI cannot call
    /// [`Self::resolve_elevation`] while a query is running.
    async fn resolve_pending_elevation(
        &mut self,
        turn: &mut TurnRuntime,
    ) -> Result<Option<ToolResult>, String> {
        let Some(elevation) = turn.pending_elevation.clone() else {
            return Ok(None);
        };

        let response = if let Some(bus) = self.permission_engine.confirmation_bus() {
            let previous_state = turn.state;
            turn.transition_to(TurnState::AwaitPermission);
            let response = bus
                .request(ConfirmationRequest {
                    id: elevation.request_id.clone(),
                    tool_name: elevation.tool_name.clone(),
                    arguments: elevation.input.to_string(),
                    path: None,
                    reason: elevation.reason.clone(),
                    allow_session: false,
                    context: HashMap::new(),
                })
                .await;
            turn.transition_to(previous_state);
            response
        } else {
            None
        };

        match response {
            Some(ConfirmationResponse::Approved | ConfirmationResponse::ApproveSession) => {
                self.resolve_elevation(turn, true).await
            }
            Some(ConfirmationResponse::Denied) => self.resolve_elevation(turn, false).await,
            None => Ok(self.abandon_pending_elevation(turn)),
        }
    }

    /// Finish an elevation that could not be presented to a user.
    ///
    /// A missing or closed confirmation channel is not a denial by the user.
    /// Return that distinction to the model instead of leaving the tool call
    /// pending forever or recording a fabricated refusal.
    fn abandon_pending_elevation(&self, turn: &mut TurnRuntime) -> Option<ToolResult> {
        let elevation = turn.pending_elevation.take()?;
        let result = ToolResult {
            tool_call_id: elevation.tool_call_id.clone(),
            output: PermissionResult::unapproved_tool_message(
                &elevation.tool_name,
                &elevation.reason,
            ),
            is_error: true,
        };
        let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
            id: elevation.tool_call_id,
            result: result.clone(),
        });
        Some(result)
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
            let result = ToolResult {
                tool_call_id: elevation.tool_call_id.clone(),
                output: format!("tool '{}' elevation denied", elevation.tool_name),
                is_error: true,
            };
            let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                id: elevation.tool_call_id,
                result: result.clone(),
            });
            return Ok(Some(result));
        }

        // Temporarily switch to BypassPermissions for the elevated call
        let checker = self.permission_engine.checker();
        let prev_mode = {
            let mut c = checker.lock().unwrap();
            let prev = c.mode();
            c.set_mode(PermissionMode::BypassPermissions);
            prev
        };

        // Fire PreToolUse hook
        let mut ctx = std::collections::HashMap::new();
        ctx.insert("tool_call_id".to_string(), elevation.tool_call_id.clone());
        ctx.insert("tool_input".to_string(), elevation.input.to_string());
        let pre_hook = self
            .fire_hook(hook_events::PRE_TOOL_USE, &elevation.tool_name, &ctx)
            .await;
        if pre_hook.stop {
            // Restore mode
            {
                let mut c = checker.lock().unwrap();
                c.set_mode(prev_mode);
            }
            let result = ToolResult {
                tool_call_id: elevation.tool_call_id.clone(),
                output: "elevated tool execution stopped by hook".to_string(),
                is_error: true,
            };
            let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                id: elevation.tool_call_id,
                result: result.clone(),
            });
            return Ok(Some(result));
        }

        // Re-run permission check (should now pass with BypassPermissions).
        // Capture the result before `?` so an evaluation error cannot leave the
        // shared checker stuck in BypassPermissions.
        let permission_result = self
            .permission_engine
            .evaluate(
                &elevation.tool_name,
                elevation.input.to_string(),
                None,
                permission_context_for(turn.agent_id.as_deref(), &elevation.tool_call_id),
            )
            .await;

        // Restore original mode
        {
            let mut c = checker.lock().unwrap();
            c.set_mode(prev_mode);
        }

        let (_request, perm_result) =
            permission_result.map_err(|e| format!("permission check failed: {e}"))?;

        let result = match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                let outcome = self
                    .tools
                    .execute(
                        &elevation.tool_call_id,
                        &elevation.tool_name,
                        &elevation.input,
                    )
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

    /// Interrupt the current turn (e.g., from Ctrl+C).
    ///
    /// Sets the flag the turn loop reads *and* emits the event: the event
    /// tells a UI the session is ending, the flag is what actually stops the
    /// work.
    pub fn interrupt(&self) {
        self.interrupt_handle().interrupt();
        let _ = self.event_tx.send(EngineEvent::SessionEnd);
    }

    /// A handle for interrupting this engine from outside it.
    ///
    /// A caller that has moved the engine into a task — the TUI does — keeps
    /// one of these instead, because it can no longer call `interrupt` on an
    /// engine it does not own.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.interrupt.clone()
    }
}

/// The context a permission decision is made in.
///
/// `BackgroundAgentDenyHook` reads `execution_mode` to refuse tools in a
/// background run, and every call site passed an empty map — so a hook that
/// was written, registered, and documented could never fire. The mode comes
/// from the run this call belongs to: one with an agent id is a forked or
/// background flow, one without is the person's own turn.
fn permission_context_for(
    agent_id: Option<&str>,
    tool_call_id: &str,
) -> std::collections::HashMap<String, String> {
    let mut context = std::collections::HashMap::new();
    context.insert(
        "execution_mode".to_string(),
        if agent_id.is_some() {
            "background".to_string()
        } else {
            "interactive".to_string()
        },
    );
    context.insert("tool_call_id".to_string(), tool_call_id.to_string());
    if let Some(agent_id) = agent_id {
        context.insert("agent_id".to_string(), agent_id.to_string());
    }
    context
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
        let mut engine = QueryEngine::new(
            Box::new(api),
            ModelId::sonnet(),
            session.clone(),
            perm_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));
        (engine, rx, session)
    }

    /// A hook executor that records the events the engine fires and their
    /// context, so tests can assert that lifecycle hooks really ran.
    struct RecordingHooks {
        events: Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>,
    }

    impl RecordingHooks {
        fn new() -> (
            Self,
            Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>,
        ) {
            let events = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    events: Arc::clone(&events),
                },
                events,
            )
        }
    }

    #[async_trait::async_trait]
    impl HookExecutor for RecordingHooks {
        async fn fire_hook(
            &self,
            event_name: &str,
            _input: &str,
            context: &HashMap<String, String>,
        ) -> HookFireResult {
            self.events
                .lock()
                .expect("the recording hook lock is not poisoned")
                .push((event_name.to_string(), context.clone()));
            HookFireResult::default()
        }
    }

    fn recorded_event_names(
        events: &Arc<std::sync::Mutex<Vec<(String, HashMap<String, String>)>>>,
    ) -> Vec<String> {
        events
            .lock()
            .expect("the recording hook lock is not poisoned")
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// A streaming client whose response consumes more than a one-token
    /// budget, so the turn loop reaches its budget-exhausted exits.
    struct BudgetStreamMock;

    #[async_trait::async_trait]
    impl ApiClient for BudgetStreamMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: "budgeted answer".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 10,
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
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::iter(vec![
                StreamEvent::UsageSummary(UsageSummary {
                    input_tokens: 10,
                    output_tokens: 50,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
                StreamEvent::StopReason("end_turn".into()),
                StreamEvent::StreamEnd,
            ]))
        }
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
            _tools: &[crate::tool_executor::ToolInfo],
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
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(StreamEvent::StreamEnd);
            Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }))
        }
    }

    struct EmptyStreamingApiClient;

    #[async_trait::async_trait]
    impl ApiClient for EmptyStreamingApiClient {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(EmptyStreamingApiClient)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: String::new(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 1,
                    output_tokens: 0,
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
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::iter([StreamEvent::StreamEnd]))
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
        // `turn.messages` is the per-request list, not the session history —
        // the session's messages live in `session.messages` and are re-extended
        // by `build_context`. Leaving them here duplicated the preamble, system
        // prompt and history once per turn.
        assert!(
            turn.messages.is_empty(),
            "reset must clear the per-request message list"
        );
    }

    /// Regression: the turn runtime is reused across the whole submit loop, so
    /// a second `build_context` for the same runtime must not see the first
    /// one's messages.
    #[test]
    fn rebuild_context_does_not_accumulate_messages() {
        let (mut engine, _rx, mut session) = make_test_engine();
        session.messages.push(Message::user("first"));

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        engine.build_context(&mut turn, &SubmitOptions::default());
        let after_first = turn.messages.len();

        // Same turn runtime reused for the next turn, as the submit loop does.
        turn.reset();
        engine.build_context(&mut turn, &SubmitOptions::default());
        let after_second = turn.messages.len();

        assert_eq!(
            after_first, after_second,
            "rebuilding context must be idempotent per turn, not append"
        );
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

    /// An interrupt stops the work rather than only announcing itself. The
    /// engine used to emit an event nothing listened for, so the turn ran to
    /// completion while the caller believed it had stopped.
    /// A model that interrupts the engine while it is answering — which is
    /// when a person actually presses Ctrl+C.
    struct InterruptingMock {
        handle: InterruptHandle,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ApiClient for InterruptingMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                handle: self.handle.clone(),
                calls: std::sync::Arc::clone(&self.calls),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The interrupt lands while the model is answering.
            self.handle.interrupt();
            Ok(ApiResponse {
                content: "a first answer".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            system: &str,
            messages: &[serde_json::Value],
            model: &ModelId,
            max_tokens: u32,
            tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
        > {
            let response = self
                .create_message(system, messages, model, max_tokens, tools)
                .await
                .expect("the interrupting mock answers");
            Box::pin(futures::stream::iter(vec![
                crate::api_client::StreamEvent::TextDelta(response.content),
                crate::api_client::StreamEvent::StopReason(response.stop_reason),
                crate::api_client::StreamEvent::StreamEnd,
            ]))
        }
    }

    /// A truncated answer ends the turn as `TokenLimitStop` — on both paths.
    /// The streaming path lost this: it passed a hardcoded `"end_turn"` as the
    /// stop reason and never checked for truncation, so a response the model
    /// ran out of room for looked exactly like a finished turn.
    #[tokio::test]
    async fn a_truncated_answer_stops_the_turn_on_the_streaming_path() {
        /// Answers, and says it ran out of room.
        struct TruncatingMock;

        #[async_trait::async_trait]
        impl ApiClient for TruncatingMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(TruncatingMock)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "a half-finished ans".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: "max_tokens".into(),
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let response = self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                    .expect("the mock answers");
                Box::pin(futures::stream::iter(vec![
                    crate::api_client::StreamEvent::TextDelta(response.content),
                    crate::api_client::StreamEvent::StopReason(response.stop_reason),
                    crate::api_client::StreamEvent::StreamEnd,
                ]))
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(TruncatingMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let streamed = engine
            .submit_streaming("write something long".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(
            matches!(streamed, TerminalReason::TokenLimitStop),
            "streaming reported {streamed:?}"
        );

        // And the blocking path agrees, which is the property that was lost.
        let blocked = engine
            .submit("write something long".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(
            matches!(blocked, TerminalReason::TokenLimitStop),
            "blocking reported {blocked:?}"
        );
    }

    /// Providers spell "cut off" two ways; both count.
    #[test]
    fn a_cut_off_answer_is_recognised_whichever_way_it_is_spelled() {
        assert!(stop_reason_eq("max_tokens", "max_tokens"));
        assert!(stop_reason_eq("length", "max_tokens"));
        assert!(!stop_reason_eq("end_turn", "max_tokens"));
        assert!(!stop_reason_eq("", "max_tokens"));

        assert!(matches!(
            QueryEngine::reason_for("max_tokens"),
            TerminalReason::TokenLimitStop
        ));
        assert!(matches!(
            QueryEngine::reason_for("length"),
            TerminalReason::TokenLimitStop
        ));
        assert!(matches!(
            QueryEngine::reason_for("end_turn"),
            TerminalReason::Completed
        ));
    }

    /// A provider outage ends the turn as `ApiUnavailable`, whatever the
    /// message says. The turn loop used to decide by reading the message —
    /// `e.starts_with("The AI service is temporarily unavailable")` — so the
    /// two halves of the engine were joined by a sentence.
    #[tokio::test]
    async fn an_unreachable_provider_is_reported_as_temporarily_unavailable() {
        /// Fails every request the way no network does.
        struct UnreachableMock;

        #[async_trait::async_trait]
        impl ApiClient for UnreachableMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(UnreachableMock)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(crate::api_client::ApiError::Transport {
                    kind: crate::api_client::TransportError::Connect,
                    class: crate::api_client::ErrorClass::Connection,
                    // Deliberately not the sentence the loop used to look for.
                    message: "the far side is not answering".to_string(),
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                match self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                {
                    Ok(response) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::TextDelta(response.content),
                        crate::api_client::StreamEvent::StreamEnd,
                    ])),
                    Err(error) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::Error(error),
                    ])),
                }
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(UnreachableMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let reason = engine
            .submit("say something".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            matches!(reason, TerminalReason::ApiUnavailable { .. }),
            "an unreachable provider came back as {reason:?}"
        );
    }

    /// And a failure that is ours is not dressed up as a provider outage.
    #[tokio::test]
    async fn a_bad_request_is_not_reported_as_a_provider_outage() {
        struct RejectingMock;

        #[async_trait::async_trait]
        impl ApiClient for RejectingMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(RejectingMock)
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Err(crate::api_client::ApiError::Response {
                    status: 400,
                    message: "messages: field required".to_string(),
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                match self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                {
                    Ok(response) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::TextDelta(response.content),
                        crate::api_client::StreamEvent::StreamEnd,
                    ])),
                    Err(error) => Box::pin(futures::stream::iter(vec![
                        crate::api_client::StreamEvent::Error(error),
                    ])),
                }
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(RejectingMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let outcome = engine
            .submit("say something".into(), SubmitOptions::default())
            .await;

        assert!(
            outcome.is_err(),
            "a bad request was reported as recoverable: {outcome:?}"
        );
    }

    #[test]
    fn a_turn_error_says_which_kind_it_is() {
        let unavailable = TurnError::ApiUnavailable {
            message: "later".into(),
        };
        assert!(matches!(
            unavailable.terminal_reason(),
            TerminalReason::ApiUnavailable { .. }
        ));
        assert_eq!(unavailable.message(), "later");

        let fatal = TurnError::Fatal {
            message: "our bug".into(),
        };
        assert!(matches!(fatal.terminal_reason(), TerminalReason::Error(_)));
        // A plain message from the engine's own layers is fatal.
        assert!(matches!(
            TurnError::from("tool failed".to_string()),
            TurnError::Fatal { .. }
        ));
    }

    /// The transition graph describes what a turn does: every step of a real
    /// turn is a legal move.
    #[test]
    fn the_legal_transitions_are_the_ones_a_turn_makes() {
        use TurnState as S;

        // The happy path, in order, twice round the tool loop.
        let path = [
            S::Initialize,
            S::BuildContext,
            S::RunPreSubmitHooks,
            S::StartRequest,
            S::StreamAssistant,
            S::ExecuteTools,
            S::ApplyToolResults,
            S::EvaluateContinuation,
            S::CompactRecovery,
            S::StartRequest,
            S::StreamAssistant,
            S::EvaluateContinuation,
            S::StopHooks,
            S::Finalize,
            S::Terminated,
        ];
        for pair in path.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "{} → {} is not allowed",
                pair[0].as_str(),
                pair[1].as_str()
            );
        }

        // Waiting for a decision is a detour, not a dead end.
        assert!(S::ExecuteTools.can_transition_to(S::AwaitPermission));
        assert!(S::AwaitPermission.can_transition_to(S::ExecuteTools));
        assert!(S::StartRequest.can_transition_to(S::AwaitElicitation));
        assert!(S::AwaitElicitation.can_transition_to(S::StreamAssistant));

        // A turn can always end.
        for state in [
            S::BuildContext,
            S::StartRequest,
            S::StreamAssistant,
            S::ExecuteTools,
            S::AwaitPermission,
            S::AwaitElicitation,
            S::ApplyToolResults,
            S::EvaluateContinuation,
            S::CompactRecovery,
            S::StopHooks,
            S::Finalize,
        ] {
            assert!(
                state.can_transition_to(S::Terminated),
                "{} cannot end",
                state.as_str()
            );
        }
    }

    /// The graph is checked against a turn that really ran, not against a path
    /// written by hand to match it.
    #[tokio::test]
    async fn a_real_turn_only_makes_legal_moves() {
        struct ToolThenText {
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for ToolThenText {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    calls: std::sync::Arc::clone(&self.calls),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ApiResponse {
                    content: if call == 0 {
                        String::new()
                    } else {
                        "done".into()
                    },
                    tool_calls: if call == 0 {
                        vec![crate::types::ToolCall {
                            id: "tc_1".into(),
                            name: "bash".into(),
                            input: serde_json::json!({"command": "true"}),
                        }]
                    } else {
                        Vec::new()
                    },
                    usage: UsageSummary {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: if call == 0 { "tool_use" } else { "end_turn" }.into(),
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let response = self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                    .expect("the mock answers");
                let mut events = Vec::new();
                if !response.content.is_empty() {
                    events.push(crate::api_client::StreamEvent::TextDelta(
                        response.content.clone(),
                    ));
                }
                events.push(crate::api_client::StreamEvent::StopReason(
                    response.stop_reason.clone(),
                ));
                for call in &response.tool_calls {
                    events.insert(
                        0,
                        crate::api_client::StreamEvent::ToolCallBlockStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    );
                    events.insert(
                        1,
                        crate::api_client::StreamEvent::ToolCallBlockEnd {
                            id: call.id.clone(),
                            input: call.input.clone(),
                        },
                    );
                }
                events.push(crate::api_client::StreamEvent::StreamEnd);
                Box::pin(futures::stream::iter(events))
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(ToolThenText {
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let _ = take_recorded_transitions();
        engine
            .submit_streaming("run something".into(), SubmitOptions::default())
            .await
            .unwrap();

        let recorded = take_recorded_transitions();
        assert!(recorded.len() > 5, "the turn barely moved: {recorded:?}");
        for (from, to) in &recorded {
            assert!(
                from.can_transition_to(*to),
                "a real turn went {} → {}, which the graph refuses",
                from.as_str(),
                to.as_str()
            );
        }
    }

    /// A real turn that goes through `CompactRecovery` also only makes legal
    /// moves. The recorder test above only exercised the tool loop; the
    /// compaction edges (`CompactRecovery -> StartRequest`,
    /// `StartRequest -> StopHooks`) are what this covers.
    #[tokio::test]
    async fn a_real_turn_through_compaction_only_makes_legal_moves() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(BudgetStreamMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);

        let _ = take_recorded_transitions();
        let reason = engine
            .execute_turn_streaming(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();
        assert_eq!(reason, TerminalReason::BudgetExhausted);

        let recorded = take_recorded_transitions();
        assert!(
            recorded
                .iter()
                .any(|(from, to)| matches!(from, TurnState::CompactRecovery)
                    && matches!(to, TurnState::StartRequest)),
            "the turn never took the compaction-retry edge: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|(_, to)| matches!(to, TurnState::StopHooks)),
            "the turn never entered the stop hooks: {recorded:?}"
        );
        for (from, to) in &recorded {
            assert!(
                from.can_transition_to(*to),
                "a real turn went {} → {}, which the graph refuses",
                from.as_str(),
                to.as_str()
            );
        }
    }

    /// A budget-exhausted turn still runs the Stop and TurnEnd hooks: it used
    /// to return straight out of the loop, past every closing step.
    #[tokio::test]
    async fn a_budget_exhausted_turn_runs_stop_and_turn_end_hooks() {
        let (mut engine, _rx, _session) = make_test_engine();
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Box::new(hooks));
        // No recovery budget, so the turn takes the direct budget exit.
        engine.compact_config.max_recovery_retries = 0;

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);

        let reason = engine
            .execute_turn(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();

        assert_eq!(reason, TerminalReason::BudgetExhausted);
        let names = recorded_event_names(&recorded);
        assert!(
            names.iter().any(|name| name == hook_events::STOP),
            "the Stop hook did not fire: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == hook_events::TURN_END),
            "the TurnEnd hook did not fire: {names:?}"
        );
        assert!(matches!(turn.state, TurnState::Terminated));
        assert_eq!(turn.terminal_reason, Some(TerminalReason::BudgetExhausted));
    }

    /// The streaming path's post-recovery over-budget exit closes through the
    /// same hooks as a normal end of turn.
    #[tokio::test]
    async fn a_budget_exhausted_streaming_turn_runs_stop_and_turn_end_hooks() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(BudgetStreamMock),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Box::new(hooks));

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);

        let reason = engine
            .execute_turn_streaming(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();

        assert_eq!(reason, TerminalReason::BudgetExhausted);
        let names = recorded_event_names(&recorded);
        assert!(
            names.iter().any(|name| name == hook_events::STOP),
            "the Stop hook did not fire: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == hook_events::TURN_END),
            "the TurnEnd hook did not fire: {names:?}"
        );
        assert_eq!(turn.terminal_reason, Some(TerminalReason::BudgetExhausted));
    }

    /// A compaction failure also closes through the hooks, from
    /// `CompactRecovery` (an edge the graph did not have).
    #[tokio::test]
    async fn a_recovery_failure_runs_stop_and_turn_end_hooks() {
        let (mut engine, _rx, _session) = make_test_engine();
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Box::new(hooks));

        let mut budget = TokenBudget::with_defaults();
        budget.thresholds.max_tokens_per_session = 1;
        let mut turn = TurnRuntime::new(budget);
        turn.compact_failures = engine.compact_config.max_consecutive_failures;

        let reason = engine
            .execute_turn(&mut turn, &SubmitOptions::default(), 1)
            .await
            .unwrap();

        assert!(matches!(reason, TerminalReason::Error(_)), "{reason:?}");
        let names = recorded_event_names(&recorded);
        assert!(
            names.iter().any(|name| name == hook_events::STOP),
            "the Stop hook did not fire: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == hook_events::TURN_END),
            "the TurnEnd hook did not fire: {names:?}"
        );
    }

    /// The exact graph, state by state.
    ///
    /// Written as the whole set rather than a few `assert!(!...)` lines: a
    /// spot check passes whether the graph is right or merely missing an
    /// entry, which is how `evaluate_continuation → stop_hooks` went unnoticed
    /// until a real turn was recorded.
    #[test]
    fn the_legal_moves_are_exactly_these() {
        use TurnState as S;

        let expected: &[(S, &[S])] = &[
            (S::Initialize, &[S::BuildContext, S::Terminated]),
            (S::BuildContext, &[S::RunPreSubmitHooks, S::Terminated]),
            (
                S::RunPreSubmitHooks,
                &[S::StartRequest, S::CompactRecovery, S::Terminated],
            ),
            (
                S::StartRequest,
                &[
                    S::StreamAssistant,
                    S::AwaitElicitation,
                    S::StopHooks,
                    S::Terminated,
                ],
            ),
            (
                S::StreamAssistant,
                &[
                    S::ExecuteTools,
                    S::EvaluateContinuation,
                    S::AwaitElicitation,
                    S::Terminated,
                ],
            ),
            (
                S::ExecuteTools,
                &[S::ApplyToolResults, S::AwaitPermission, S::Terminated],
            ),
            (
                S::AwaitPermission,
                &[S::ExecuteTools, S::ApplyToolResults, S::Terminated],
            ),
            (
                S::AwaitElicitation,
                &[S::StreamAssistant, S::StartRequest, S::Terminated],
            ),
            (
                S::ApplyToolResults,
                &[S::EvaluateContinuation, S::Terminated],
            ),
            (
                S::EvaluateContinuation,
                &[S::CompactRecovery, S::StopHooks, S::Terminated],
            ),
            (
                S::CompactRecovery,
                &[S::StartRequest, S::StopHooks, S::Finalize, S::Terminated],
            ),
            (S::StopHooks, &[S::Finalize, S::Terminated]),
            (S::Finalize, &[S::Terminated]),
            (S::Terminated, &[S::Terminated]),
        ];

        let all = [
            S::Initialize,
            S::BuildContext,
            S::RunPreSubmitHooks,
            S::StartRequest,
            S::StreamAssistant,
            S::ExecuteTools,
            S::AwaitPermission,
            S::AwaitElicitation,
            S::ApplyToolResults,
            S::EvaluateContinuation,
            S::CompactRecovery,
            S::StopHooks,
            S::Finalize,
            S::Terminated,
        ];

        for (from, allowed) in expected {
            for to in all {
                assert_eq!(
                    from.can_transition_to(to),
                    allowed.contains(&to),
                    "{} → {} should be {}",
                    from.as_str(),
                    to.as_str(),
                    if allowed.contains(&to) {
                        "allowed"
                    } else {
                        "refused"
                    }
                );
            }
        }
    }

    #[test]
    fn an_illegal_transition_is_reported_and_does_not_stop_the_turn() {
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        // Nothing legitimate goes straight from a fresh turn to the end.
        assert!(!turn.state.can_transition_to(TurnState::Finalize));
        turn.transition_to(TurnState::Finalize);

        // It moved, and the turn is not poisoned by the report.
        assert!(matches!(turn.state, TurnState::Finalize));
        turn.transition_to(TurnState::Terminated);
        assert!(matches!(turn.state, TurnState::Terminated));
    }

    /// `interrupt()` sets the flag the turn loop reads, not only the event.
    /// The event is what a UI listens for; the flag is what stops the work.
    #[tokio::test]
    async fn interrupting_the_engine_sets_the_flag_it_reads() {
        let (engine, _rx, _session) = make_test_engine();
        assert!(!engine.interrupt_handle().is_interrupted());

        engine.interrupt();

        assert!(engine.interrupt_handle().is_interrupted());
    }

    /// An interrupt that lands *between* turns — while a tool is running —
    /// stops the run before the next request is sent. This is the window the
    /// loop-top check exists for: without it the engine would ask the model
    /// for another answer after the person had asked it to stop.
    #[tokio::test]
    async fn an_interrupt_between_turns_stops_before_the_next_request() {
        /// Answers with a tool call once, then with text.
        struct ToolThenText {
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for ToolThenText {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    calls: std::sync::Arc::clone(&self.calls),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ApiResponse {
                    content: if call == 0 {
                        String::new()
                    } else {
                        "done".into()
                    },
                    tool_calls: if call == 0 {
                        vec![crate::types::ToolCall {
                            id: "tc_1".into(),
                            name: "bash".into(),
                            input: serde_json::json!({"command": "true"}),
                        }]
                    } else {
                        Vec::new()
                    },
                    usage: UsageSummary {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                    stop_reason: if call == 0 { "tool_use" } else { "end_turn" }.into(),
                })
            }

            async fn create_message_streaming(
                &self,
                system: &str,
                messages: &[serde_json::Value],
                model: &ModelId,
                max_tokens: u32,
                tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let response = self
                    .create_message(system, messages, model, max_tokens, tools)
                    .await
                    .expect("the mock answers");
                let mut events = Vec::new();
                if !response.content.is_empty() {
                    events.push(crate::api_client::StreamEvent::TextDelta(
                        response.content.clone(),
                    ));
                }
                events.push(crate::api_client::StreamEvent::StopReason(
                    response.stop_reason.clone(),
                ));
                for call in &response.tool_calls {
                    // A call needs its start, its input, and its end: the
                    // assembler builds the call from the whole sequence.
                    events.insert(
                        0,
                        crate::api_client::StreamEvent::ToolCallBlockStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    );
                    events.insert(
                        1,
                        crate::api_client::StreamEvent::ToolCallBlockEnd {
                            id: call.id.clone(),
                            input: call.input.clone(),
                        },
                    );
                }
                events.push(crate::api_client::StreamEvent::StreamEnd);
                Box::pin(futures::stream::iter(events))
            }
        }

        /// Interrupts the engine when the tool runs — the person pressing
        /// Ctrl+C while something is executing.
        struct InterruptingTools(InterruptHandle);

        #[async_trait::async_trait]
        impl crate::tool_executor::ToolExecutor for InterruptingTools {
            async fn execute(
                &self,
                tool_call_id: &str,
                _tool_name: &str,
                _input: &serde_json::Value,
            ) -> crate::tool_executor::ToolDispatchOutcome {
                self.0.interrupt();
                crate::tool_executor::ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output: "the tool ran".to_string(),
                        is_error: false,
                    },
                }
            }

            fn list_tools(&self) -> Vec<crate::tool_executor::ToolInfo> {
                Vec::new()
            }
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut engine = QueryEngine::new(
            Box::new(ToolThenText {
                calls: std::sync::Arc::clone(&calls),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(InterruptingTools(engine.interrupt_handle())));

        let reason = engine
            .submit_streaming("run something".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            matches!(reason, TerminalReason::UserInterrupted),
            "expected an interrupt, got {reason:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the model was asked for another answer after the interrupt"
        );
    }

    #[tokio::test]
    async fn an_interrupt_ends_the_turn_it_arrives_during() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(MockApiClient),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let handle = engine.interrupt_handle();
        engine.set_api_client(Box::new(InterruptingMock {
            handle,
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));

        let reason = engine
            .submit_streaming("say something".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            matches!(reason, TerminalReason::UserInterrupted),
            "expected an interrupt, got {reason:?}"
        );
    }

    /// And the next turn runs: a request that arrives after an interrupt is a
    /// fresh one, not a continuation of the stopped one.
    #[tokio::test]
    async fn a_turn_after_an_interrupt_still_runs() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.interrupt();
        let _ = engine
            .submit_streaming("first".into(), SubmitOptions::default())
            .await;

        let reason = engine
            .submit_streaming("second".into(), SubmitOptions::default())
            .await
            .unwrap();

        assert!(
            !matches!(reason, TerminalReason::UserInterrupted),
            "the interrupt outlived the turn it was meant for: {reason:?}"
        );
    }

    /// An interrupt that lands before the tools run stops them: the person
    /// asked for the turn to end, not for one more batch of edits.
    #[tokio::test]
    async fn an_interrupt_stops_the_tools_from_running() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "touch /tmp/should-not-exist"}),
        });

        engine.interrupt_handle().interrupt();
        let results = engine
            .execute_tool_calls(&mut turn, &SubmitOptions::default())
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(
            results[0].output.contains("interrupted"),
            "{}",
            results[0].output
        );
    }

    /// `TurnState::StopHooks` is entered before a turn closes, and the Stop
    /// hook fires in it — the state and the event were both declared, and
    /// neither happened.
    #[tokio::test]
    async fn a_finished_turn_runs_the_stop_hooks() {
        let (mut engine, _rx, _session) = make_test_engine();
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        engine.run_stop_hooks(&mut turn, 1, "end_turn").await;

        assert!(
            matches!(turn.state, TurnState::StopHooks),
            "{:?}",
            turn.state
        );
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
                _tools: &[crate::tool_executor::ToolInfo],
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
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
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
        assert!(
            found_notification,
            "expected Notification event to be emitted"
        );
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

    #[tokio::test]
    async fn empty_streaming_response_is_not_persisted_as_assistant_message() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let perm_engine = PermissionEngine::new(checker);
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let mut engine = QueryEngine::new(
            Box::new(EmptyStreamingApiClient),
            ModelId::sonnet(),
            session,
            perm_engine,
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));

        engine
            .submit_streaming("first".into(), SubmitOptions::default())
            .await
            .unwrap();
        engine
            .submit_streaming("second".into(), SubmitOptions::default())
            .await
            .unwrap();

        let roles = engine
            .session
            .messages
            .iter()
            .map(|msg| msg.role)
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![crate::types::Role::User, crate::types::Role::User]
        );
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
        let validator = StructuredOutputValidator::new(schema).with_max_retries(2);
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

    /// The wiring, not just the helper: a turn that belongs to an agent runs
    /// its tools through the permission engine with a background context, and
    /// the hook that refuses tools in a background run refuses this one.
    /// The wiring, not just the helper: a turn that belongs to an agent runs
    /// its tools with a background context, and the hook that refuses tools in
    /// a background run refuses this one. The same call in the person's own
    /// turn is allowed.
    #[tokio::test]
    async fn a_background_turn_has_its_tools_denied_by_the_hook() {
        let (mut engine, _rx, _session) = make_test_engine();
        // As the session does.
        engine.permission_engine.add_hook(std::sync::Arc::new(
            crate::permission_engine::BackgroundAgentDenyHook,
        ));
        let budget = TokenBudget::with_defaults();

        let background = SubmitOptions {
            agent_id: Some("task_7".into()),
            ..SubmitOptions::default()
        };
        let mut turn = TurnRuntime::new(budget.clone());
        turn.agent_id = background.agent_id.clone();
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "whoami"}),
        });

        let denied = engine
            .execute_tool_calls(&mut turn, &background)
            .await
            .unwrap();
        assert_eq!(denied.len(), 1);
        assert!(denied[0].is_error, "{:?}", denied[0]);
        assert!(
            denied[0].output.contains("background"),
            "expected a denial naming the background run, got {}",
            denied[0].output
        );

        // The same call in the person's own turn runs.
        let interactive = SubmitOptions::default();
        let mut turn = TurnRuntime::new(budget);
        turn.pending_tool_calls.push(PendingToolCall {
            id: "tc_2".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "whoami"}),
        });

        let allowed = engine
            .execute_tool_calls(&mut turn, &interactive)
            .await
            .unwrap();
        assert_eq!(allowed.len(), 1);
        assert!(!allowed[0].is_error, "{:?}", allowed[0]);
    }

    /// The hook that refuses tools in a background run reads `execution_mode`
    /// from the permission context. Every call site used to hand it an empty
    /// map, so it could never fire; these pin what it is handed now.
    #[test]
    fn a_permission_context_says_which_run_it_belongs_to() {
        let interactive = permission_context_for(None, "tc_1");
        assert_eq!(interactive["execution_mode"], "interactive");
        assert_eq!(interactive["tool_call_id"], "tc_1");
        assert!(!interactive.contains_key("agent_id"));

        let background = permission_context_for(Some("task_7"), "tc_2");
        assert_eq!(background["execution_mode"], "background");
        assert_eq!(background["agent_id"], "task_7");
    }

    /// And the hook acts on it.
    #[tokio::test]
    async fn the_background_deny_hook_fires_when_the_run_is_a_background_one() {
        use crate::permission_engine::PermissionRequest;
        use crate::permission_engine::{BackgroundAgentDenyHook, PermissionHook};
        use crate::permissions::PermissionMode;

        let hook = BackgroundAgentDenyHook;
        let request = |context: std::collections::HashMap<String, String>| PermissionRequest {
            id: "req_1".into(),
            tool_name: "Bash".into(),
            arguments: "{}".into(),
            path: None,
            context,
            created_at: chrono::Utc::now(),
        };

        let denied = hook
            .evaluate(
                &request(permission_context_for(Some("task_7"), "tc_1")),
                PermissionMode::Default,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                denied,
                Some(crate::permissions::PermissionResult::Deny { .. })
            ),
            "{denied:?}"
        );

        let allowed = hook
            .evaluate(
                &request(permission_context_for(None, "tc_1")),
                PermissionMode::Default,
            )
            .await
            .unwrap();
        assert!(allowed.is_none(), "{allowed:?}");
    }

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
            reason: "test elevation".into(),
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
            reason: "test elevation".into(),
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

    #[tokio::test]
    async fn execute_tool_calls_drives_every_elevation_through_the_confirmation_bus() {
        let (mut engine, mut rx, _session) = make_test_engine();
        engine
            .permission_checker()
            .lock()
            .unwrap()
            .set_mode(PermissionMode::Default);
        let bus = ConfirmationBus::new(4);
        engine.set_confirmation_bus(bus.clone());

        let responder = tokio::spawn(async move {
            for expected_id in ["elev_tc_one", "elev_tc_two"] {
                let request = bus.next_request().await.expect("elevation request");
                assert_eq!(request.id, expected_id);
                assert!(!request.allow_session);
                assert!(request.reason.contains("sudo"));
                assert!(
                    bus.respond(&request.id, ConfirmationResponse::Approved)
                        .await
                );
            }
        });

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.pending_tool_calls = vec![
            PendingToolCall {
                id: "tc_one".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "sudo whoami"}),
            },
            PendingToolCall {
                id: "tc_two".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "sudo id"}),
            },
        ];

        let results = engine
            .execute_tool_calls(&mut turn, &SubmitOptions::default())
            .await
            .unwrap();
        responder.await.unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| !result.is_error));
        assert!(turn.pending_elevation.is_none());
        assert_eq!(
            engine.permission_checker().lock().unwrap().mode(),
            PermissionMode::Default
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ToolCallStart { .. }))
                .count(),
            2,
            "retries must not duplicate ToolCallStart: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ElevationRequest { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ToolCallEnd { .. }))
                .count(),
            2,
            "each elevated call must finish exactly once: {events:?}"
        );
    }

    #[tokio::test]
    async fn elevation_without_a_confirmation_channel_is_not_a_user_denial() {
        let (mut engine, mut rx, _session) = make_test_engine();
        engine
            .permission_checker()
            .lock()
            .unwrap()
            .set_mode(PermissionMode::Default);
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.pending_tool_calls = vec![PendingToolCall {
            id: "tc_no_bus".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "sudo whoami"}),
        }];

        let results = engine
            .execute_tool_calls(&mut turn, &SubmitOptions::default())
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(results[0].output.contains("no confirmation channel"));
        assert!(!results[0].output.contains("denied by user"));
        assert!(engine.denied_tools().is_empty());
        assert!(turn.pending_elevation.is_none());

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EngineEvent::ToolCallEnd { .. }))
                .count(),
            1,
            "an unresolved call still needs a terminal event: {events:?}"
        );
    }

    #[tokio::test]
    async fn early_streaming_elevation_waits_for_approval_and_executes() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine
            .permission_checker()
            .lock()
            .unwrap()
            .set_mode(PermissionMode::Default);
        let bus = ConfirmationBus::new(1);
        engine.set_confirmation_bus(bus.clone());
        let responder = tokio::spawn(async move {
            let request = bus.next_request().await.expect("elevation request");
            assert_eq!(request.id, "elev_tc_early");
            bus.respond(&request.id, ConfirmationResponse::Approved)
                .await;
        });
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        let result = engine
            .execute_early_tool_call(
                &mut turn,
                ToolCall {
                    id: "tc_early".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "sudo whoami"}),
                },
                &SubmitOptions::default(),
            )
            .await
            .unwrap()
            .expect("resolved tool result");
        responder.await.unwrap();

        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output, "stub ok: bash");
        assert!(turn.pending_elevation.is_none());
    }

    #[test]
    fn pending_elevation_struct_fields() {
        let elev = PendingElevation {
            tool_call_id: "tc_1".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({}),
            request_id: "elev_1".into(),
            reason: "test elevation".into(),
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

    #[test]
    fn provider_messages_map_tool_result_to_user_role() {
        let tool_call = ToolCall {
            id: "tc_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
        };
        let tool_result = ToolResult {
            tool_call_id: "tc_1".into(),
            output: "file body".into(),
            is_error: false,
        };
        let messages = QueryEngine::provider_messages(&[
            Message::user("Read src/lib.rs"),
            Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tool_call),
            },
            Message::tool(tool_result),
        ]);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "tc_1");
    }

    /// The context-size line is checked before a request, not only after an
    /// output-token budget trips. A small-window model with a large input must
    /// compact before sending.
    #[tokio::test]
    async fn the_context_threshold_compacts_before_a_request() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(MockApiClient),
            ModelId("unknown-small-window".into()),
            SessionState::new(std::path::PathBuf::from("/tmp/test")),
            PermissionEngine::new(checker),
            tx,
        );
        engine.set_tool_executor(Box::new(StubOkToolExecutor));

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages
            .push(Message::user("the original request that must be preserved"));
        for i in 0..10 {
            turn.messages.push(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::Text(format!(
                    "assistant turn {i}: {}",
                    "x".repeat(10_000)
                )),
            });
        }

        let before = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert!(before > 16_000, "the test context is too small: {before}");

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(
            reason.is_none(),
            "an over-line context was refused: {reason:?}"
        );

        let after = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert!(
            after < before,
            "the context was not reduced: {before} -> {after}"
        );
        assert_eq!(turn.recovery_count, 1, "the compaction was not recorded");
    }

    /// `Reactive` compaction is reachable from the engine, not dead code: a
    /// response that stopped on `max_tokens` while the context is over the
    /// compact line selects it.
    #[tokio::test]
    async fn a_max_tokens_stop_over_the_line_selects_reactive_compaction() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(MockApiClient),
            ModelId("unknown-small-window".into()),
            SessionState::new(std::path::PathBuf::from("/tmp/test")),
            PermissionEngine::new(checker),
            tx,
        );
        let (hooks, recorded) = RecordingHooks::new();
        engine.set_hooks(Box::new(hooks));

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.last_stop_reason = Some("max_tokens".into());
        turn.messages
            .push(Message::user("the original request that must be preserved"));
        for i in 0..10 {
            turn.messages.push(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::Text(format!(
                    "assistant turn {i}: {}",
                    "x".repeat(10_000)
                )),
            });
        }

        let before = PromptCompiler::estimate_message_tokens(&turn.messages);
        assert!(before > 16_000, "the test context is too small: {before}");

        let reason = engine
            .maybe_compact_on_threshold(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(reason.is_none(), "the turn was blocked: {reason:?}");

        let events = recorded
            .lock()
            .expect("the recording hook lock is not poisoned");
        let strategy = events
            .iter()
            .find(|(name, _)| name == hook_events::PRE_COMPACT)
            .and_then(|(_, context)| context.get("strategy"))
            .cloned();
        assert_eq!(
            strategy.as_deref(),
            Some("Reactive"),
            "max_tokens did not select reactive compaction: {events:?}"
        );
    }

    #[test]
    fn persist_assistant_tool_calls_records_tool_use_messages() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        let tool_call = ToolCall {
            id: "tc_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
        };

        engine.persist_assistant_tool_calls(&mut turn, std::slice::from_ref(&tool_call));

        assert!(matches!(
            turn.messages.first(),
            Some(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tc),
            }) if tc.id == "tc_1"
        ));
        assert!(matches!(
            engine.session.messages.last(),
            Some(Message {
                role: crate::types::Role::Assistant,
                content: crate::types::MessageContent::ToolCall(tc),
            }) if tc.id == "tc_1"
        ));
    }

    // ─── Elicitation Integration ───────────────────────────────────────────

    #[tokio::test]
    async fn handle_elicitation_stores_response() {
        let (mut engine, _rx, _session) = make_test_engine();
        engine.set_elicitation_handler(Box::new(
            crate::elicitation::SdkElicitationHandler::default(),
        ));
        let budget = TokenBudget::with_defaults();
        let mut turn = TurnRuntime::new(budget);

        let result = engine
            .handle_elicitation(&mut turn, "elic_1".into(), "Which approach?".into())
            .await;
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
        engine
            .set_elicitation_handler(Box::new(crate::elicitation::SdkElicitationHandler::new(tx)));
        // Handler was set without error
    }

    #[tokio::test]
    async fn elicitation_heuristic_triggers_in_execute_turn() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        struct StatefulMock {
            call_count: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for StatefulMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(StatefulMock {
                    call_count: Arc::clone(&self.call_count),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
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
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>>
            {
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
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        let mut engine =
            QueryEngine::new(Box::new(api), ModelId::sonnet(), session, perm_engine, tx);
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
                (crate::types::Role::User, crate::types::MessageContent::Text(t)) => {
                    Some(t.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            user_texts.iter().any(|t| t == "Approach A"),
            "expected user clarification in turn messages"
        );

        let assistant_texts: Vec<String> = turn
            .messages
            .iter()
            .filter_map(|m| match (&m.role, &m.content) {
                (crate::types::Role::Assistant, crate::types::MessageContent::Text(t)) => {
                    Some(t.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant_texts.len(),
            2,
            "expected two assistant messages: question and answer"
        );
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

        let messages = vec![Message::user("hello"), Message::assistant("world")];

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
        QueryEngine::write_transcript_replacement(
            &path,
            &[
                Message::user("old1"),
                Message::user("old2"),
                Message::user("old3"),
            ],
        )
        .unwrap();

        // Replace with fewer messages
        QueryEngine::write_transcript_replacement(&path, &[Message::user("new")]).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn append_transcript_boundary_adds_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.jsonl");

        QueryEngine::write_transcript_replacement(&path, &[Message::user("before")]).unwrap();

        QueryEngine::append_transcript_boundary(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("coderex_compact_boundary"));
    }

    // ─── Provider stop reasons ─────────────────────────────────────────────

    /// A stop reason is read for what it asks the loop to do.
    #[test]
    fn a_stop_reason_says_what_the_loop_does_next() {
        assert_eq!(classify_stop_reason("tool_use"), StopDisposition::RunTools);
        assert_eq!(
            classify_stop_reason("tool_calls"),
            StopDisposition::RunTools
        );
        assert_eq!(classify_stop_reason("pause_turn"), StopDisposition::Resume);
        assert_eq!(classify_stop_reason("compaction"), StopDisposition::Resume);

        // Anything not named is the end of the turn — including a reason a
        // provider invents next, which must not make the loop re-request.
        for ending in ["end_turn", "max_tokens", "length", "refusal", "stop", ""] {
            assert_eq!(
                classify_stop_reason(ending),
                StopDisposition::Stop,
                "{ending:?}"
            );
        }
    }

    /// A paused turn is reported as one to resume, not as a finished answer.
    /// `reason_for` used to special-case only `max_tokens`; every other reason
    /// became `Completed`, so a `pause_turn` ended the run mid-answer.
    #[test]
    fn a_paused_turn_is_not_reported_as_finished() {
        for paused in ["pause_turn", "compaction"] {
            assert!(
                matches!(QueryEngine::reason_for(paused), TerminalReason::Resume),
                "{paused} should resume"
            );
        }
        for finished in ["end_turn", "refusal", "stop", ""] {
            assert!(
                matches!(QueryEngine::reason_for(finished), TerminalReason::Completed),
                "{finished} should finish"
            );
        }
        assert!(matches!(
            QueryEngine::reason_for("max_tokens"),
            TerminalReason::TokenLimitStop
        ));
    }

    /// A resumed turn runs again, but a provider that only ever pauses cannot
    /// spin the loop forever.
    #[test]
    fn a_paused_turn_runs_again_until_the_bound() {
        assert!(matches!(
            next_loop_step(&TerminalReason::Resume, false, 0),
            LoopStep::Continue
        ));
        assert!(matches!(
            next_loop_step(&TerminalReason::Resume, false, MAX_PROVIDER_RESUMES - 1),
            LoopStep::Continue
        ));
        assert!(matches!(
            next_loop_step(&TerminalReason::Resume, false, MAX_PROVIDER_RESUMES),
            LoopStep::Break(TerminalReason::Completed)
        ));

        // The older rule still holds: a turn that ended on a tool result is
        // not finished, the model has not seen the result yet.
        assert!(matches!(
            next_loop_step(&TerminalReason::Completed, true, 0),
            LoopStep::Continue
        ));
        assert!(matches!(
            next_loop_step(&TerminalReason::Completed, false, 0),
            LoopStep::Break(TerminalReason::Completed)
        ));
    }

    /// Answers with the next stop reason from a fixed list, one per request,
    /// and counts the requests.
    struct StopReasonSequenceMock {
        reasons: Vec<String>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl StopReasonSequenceMock {
        fn next_reason(&self) -> String {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.reasons
                .get(n)
                .or_else(|| self.reasons.last())
                .cloned()
                .unwrap_or_else(|| "end_turn".to_string())
        }
    }

    #[async_trait::async_trait]
    impl ApiClient for StopReasonSequenceMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                reasons: self.reasons.clone(),
                calls: Arc::clone(&self.calls),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[crate::tool_executor::ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            let n = self.calls.load(std::sync::atomic::Ordering::SeqCst);
            Ok(ApiResponse {
                content: format!("answer {n}"),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: self.next_reason(),
            })
        }

        async fn create_message_streaming(
            &self,
            system: &str,
            messages: &[serde_json::Value],
            model: &ModelId,
            max_tokens: u32,
            tools: &[crate::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
        > {
            let response = self
                .create_message(system, messages, model, max_tokens, tools)
                .await
                .expect("the sequence mock answers");
            Box::pin(futures::stream::iter(vec![
                crate::api_client::StreamEvent::TextDelta(response.content),
                crate::api_client::StreamEvent::StopReason(response.stop_reason),
                crate::api_client::StreamEvent::StreamEnd,
            ]))
        }
    }

    fn sequence_engine(reasons: &[&str]) -> (QueryEngine, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let engine = QueryEngine::new(
            Box::new(StopReasonSequenceMock {
                reasons: reasons.iter().map(|r| (*r).to_string()).collect(),
                calls: Arc::clone(&calls),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );
        (engine, calls)
    }

    /// A `pause_turn` is a pause, not an ending: the request is sent again and
    /// the run reports the reason that really ended it.
    #[tokio::test]
    async fn a_paused_turn_is_resumed_rather_than_reported_as_an_answer() {
        let (mut engine, calls) = sequence_engine(&["pause_turn", "end_turn"]);
        let reason = engine
            .submit("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the paused turn should have been requested again"
        );
    }

    /// The streaming path resumes a paused turn too.
    #[tokio::test]
    async fn a_paused_turn_is_resumed_on_the_streaming_path() {
        let (mut engine, calls) = sequence_engine(&["compaction", "end_turn"]);
        let reason = engine
            .submit_streaming("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A provider that only ever pauses stops at the bound rather than
    /// spinning.
    #[tokio::test]
    async fn a_provider_that_only_pauses_does_not_loop_forever() {
        let (mut engine, calls) = sequence_engine(&["pause_turn"]);
        let reason = engine
            .submit("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_PROVIDER_RESUMES as usize + 1
        );
    }

    // ─── Mid-stream interruption recovery ──────────────────────────────────

    /// Merge keeps a prefix from being delivered twice and never loses text.
    #[test]
    fn a_retry_that_repeats_the_opening_does_not_repeat_it_to_the_reader() {
        assert_eq!(
            merge_stream_text("The answer is ", "The answer is 42."),
            "The answer is 42."
        );
        assert_eq!(merge_stream_text("abc", "ab"), "abc");
        assert_eq!(merge_stream_text("", "abc"), "abc");
        // A retry that picks up past the cut, or diverges, keeps both — text
        // already delivered is never dropped.
        assert_eq!(merge_stream_text("abc", "def"), "abcdef");
    }

    /// The incremental merger skips an opening the retry repeats, then emits
    /// the rest once — it does not re-append the attempt's prefix per delta.
    #[test]
    fn an_identical_retry_opening_is_merged_incrementally() {
        let mut merger = StreamTextMerger::new();
        assert_eq!(
            merger.push_delta("The answer is ").as_deref(),
            Some("The answer is ")
        );

        merger.begin_attempt();
        assert_eq!(
            merger.push_delta("The answer is 42.").as_deref(),
            Some("42.")
        );
        assert_eq!(merger.delivered(), "The answer is 42.");
    }

    /// A retry that diverges immediately hands back only each newly arrived
    /// fragment: `x`, then `y`, then `z`, never `x`, `xy`, `xyz`.
    #[test]
    fn a_divergent_retry_is_merged_incrementally() {
        let mut merger = StreamTextMerger::new();
        assert_eq!(merger.push_delta("abc").as_deref(), Some("abc"));

        merger.begin_attempt();
        assert_eq!(merger.push_delta("x").as_deref(), Some("x"));
        assert_eq!(merger.push_delta("y").as_deref(), Some("y"));
        assert_eq!(merger.push_delta("z").as_deref(), Some("z"));

        assert_eq!(merger.delivered(), "abcxyz");
    }

    /// A stream that dies mid-answer is re-requested, and what it already
    /// delivered reaches the reader once. The old recovery re-requested from
    /// scratch and threw the partial text away.
    #[tokio::test]
    async fn a_stream_that_dies_mid_answer_keeps_what_it_delivered() {
        struct InterruptedStreamMock {
            attempts: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for InterruptedStreamMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    attempts: Arc::clone(&self.attempts),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "fallback".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 1,
                        output_tokens: 1,
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
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let n = self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // The connection dies with the answer half-delivered.
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("The answer is ".into()),
                        StreamEvent::Error(crate::api_client::ApiError::Transport {
                            kind: crate::api_client::TransportError::Other,
                            class: crate::api_client::ErrorClass::Stream,
                            message: "connection reset mid-body".into(),
                        }),
                    ]))
                } else {
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("The answer is 42.".into()),
                        StreamEvent::StopReason("end_turn".into()),
                        StreamEvent::StreamEnd,
                    ]))
                }
            }
        }

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(InterruptedStreamMock {
                attempts: Arc::clone(&attempts),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let reason = engine
            .submit_streaming("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the cut stream should have been re-requested"
        );

        let mut delivered = String::new();
        let mut interrupts: Vec<(bool, usize)> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                EngineEvent::TextDelta(text) => delivered.push_str(&text),
                EngineEvent::StreamInterrupted {
                    will_retry,
                    partial_len,
                    ..
                } => interrupts.push((will_retry, partial_len)),
                _ => {}
            }
        }

        assert_eq!(
            delivered, "The answer is 42.",
            "the reader should see the answer once, not twice"
        );
        assert_eq!(
            interrupts,
            vec![(true, "The answer is ".len())],
            "the interruption should be reported as one that will retry"
        );
    }

    /// A retry that diverges after a partial answer emits each new fragment
    /// once. The old per-delta merge re-applied the accumulated attempt, so
    /// the reader saw `x`, then `xy`, then `xyz` — and the output grew
    /// quadratically with the retry's length.
    #[tokio::test]
    async fn a_divergent_retry_emits_each_fragment_once() {
        struct DivergingRetryMock {
            attempts: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for DivergingRetryMock {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    attempts: Arc::clone(&self.attempts),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &ModelId,
                _max_tokens: u32,
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, crate::api_client::ApiError> {
                Ok(ApiResponse {
                    content: "fallback".into(),
                    tool_calls: Vec::new(),
                    usage: UsageSummary {
                        input_tokens: 1,
                        output_tokens: 1,
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
                _tools: &[crate::tool_executor::ToolInfo],
            ) -> std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::api_client::StreamEvent> + Send + 'static>,
            > {
                let n = self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("abc".into()),
                        StreamEvent::Error(crate::api_client::ApiError::Transport {
                            kind: crate::api_client::TransportError::Other,
                            class: crate::api_client::ErrorClass::Stream,
                            message: "connection reset mid-body".into(),
                        }),
                    ]))
                } else {
                    Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta("x".into()),
                        StreamEvent::TextDelta("y".into()),
                        StreamEvent::TextDelta("z".into()),
                        StreamEvent::StopReason("end_turn".into()),
                        StreamEvent::StreamEnd,
                    ]))
                }
            }
        }

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = SessionState::new(std::path::PathBuf::from("/tmp/test"));
        let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
        let mut engine = QueryEngine::new(
            Box::new(DivergingRetryMock {
                attempts: Arc::clone(&attempts),
            }),
            ModelId::sonnet(),
            session,
            PermissionEngine::new(checker),
            tx,
        );

        let reason = engine
            .submit_streaming("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);

        let mut deltas: Vec<String> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::TextDelta(text) = event {
                deltas.push(text);
            }
        }

        assert_eq!(
            deltas,
            vec!["abc".to_string(), "x".into(), "y".into(), "z".into()],
            "the retry's fragments were replayed: {deltas:?}"
        );
        assert_eq!(
            deltas.concat(),
            "abcxyz",
            "the answer grew by more than the retry's text"
        );
    }

    // ─── Compaction outcomes ───────────────────────────────────────────────

    /// Why compaction did not happen is returned, not only logged.
    #[tokio::test]
    async fn compaction_says_why_it_did_not_happen() {
        let (engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];
        turn.compact_failures = engine.compact_config.max_consecutive_failures;

        let outcome = engine
            .run_compact_recovery(
                &mut turn,
                &SubmitOptions::default(),
                1,
                CompactTrigger::TokenThreshold,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::CircuitBreakerOpen { .. }),
            "{outcome:?}"
        );
        assert!(outcome.failure_message().is_some());
    }

    /// Only the outcomes that end the turn carry a message.
    #[test]
    fn only_a_fatal_compaction_outcome_ends_the_turn() {
        let compacted = CompactOutcome::Compacted {
            strategy: CompactStrategy::Micro,
            trigger: CompactTrigger::TokenThreshold,
            tokens_removed: 10,
            messages_remaining: 2,
            summary: SummaryOutcome::Summarized,
        };
        assert!(compacted.failure_message().is_none());

        let deferred = CompactOutcome::DeferredToThresholdPath {
            estimated_tokens: 200_000,
        };
        assert!(deferred.failure_message().is_none());

        let stopped = CompactOutcome::StopHookTerminated {
            reason: "stop".into(),
        };
        assert_eq!(stopped.failure_message().as_deref(), Some("stop"));
    }

    // ─── Pre-compaction backup ─────────────────────────────────────────────

    /// The transcript is copied aside before compaction overwrites it, and a
    /// second compaction does not overwrite the first backup.
    #[test]
    fn a_backup_keeps_what_compaction_is_about_to_discard() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        QueryEngine::write_transcript_replacement(&path, &[Message::user("the history")]).unwrap();

        let first = QueryEngine::write_transcript_backup(&path)
            .unwrap()
            .expect("a backup");
        assert!(first.exists());
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            std::fs::read_to_string(&path).unwrap()
        );

        std::thread::sleep(std::time::Duration::from_millis(2));
        QueryEngine::write_transcript_replacement(&path, &[Message::user("compacted")]).unwrap();
        let second = QueryEngine::write_transcript_backup(&path)
            .unwrap()
            .expect("a second backup");

        assert_ne!(first, second, "the first backup must survive a second");
        assert!(std::fs::read_to_string(&first)
            .unwrap()
            .contains("the history"));
    }

    /// There is nothing to back up before the first write.
    #[test]
    fn there_is_nothing_to_back_up_before_the_first_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        assert!(QueryEngine::write_transcript_backup(&path)
            .unwrap()
            .is_none());
    }

    /// Compacting a session with a transcript on disk leaves a backup holding
    /// the pre-compaction history.
    #[tokio::test]
    async fn compaction_backs_up_the_transcript_before_overwriting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut engine, _rx, session) = make_test_engine();
        engine.set_transcript_path(tmp.path().to_path_buf());

        let transcript = tmp.path().join(format!("{}.jsonl", session.session_id));
        QueryEngine::write_transcript_replacement(&transcript, &[Message::user("the history")])
            .unwrap();

        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("a much longer history than this")];

        let outcome = engine
            .finish_compaction(
                &mut turn,
                CompactResult {
                    strategy: CompactStrategy::Micro,
                    messages: vec![Message::user("short")],
                    tokens_removed: 0,
                    boundary_inserted: false,
                    boundary_index: None,
                },
                CompactTrigger::TokenThreshold,
                SummaryOutcome::Summarized,
            )
            .await;

        assert!(
            matches!(outcome, CompactOutcome::Compacted { .. }),
            "{outcome:?}"
        );
        assert!(
            std::fs::read_to_string(&transcript)
                .unwrap()
                .contains("short"),
            "the live transcript should hold the compacted messages"
        );

        let backups: Vec<PathBuf> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".backup."))
            .collect();
        assert_eq!(backups.len(), 1, "one backup, got {backups:?}");
        assert!(
            std::fs::read_to_string(&backups[0])
                .unwrap()
                .contains("the history"),
            "the backup should hold the pre-compaction transcript"
        );
    }

    // ─── Cache-TTL compaction ──────────────────────────────────────────────

    /// A cold cache is not by itself a reason to compact a small context:
    /// nothing has gone wrong, and the pass is skipped without a model call.
    #[tokio::test]
    async fn a_warm_cache_is_not_a_reason_to_compact() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];

        // Never sent a request — no TTL to have elapsed.
        assert!(engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await
            .is_none());

        // A request a moment ago leaves the cache warm.
        engine.last_request_at = Some(Instant::now());
        assert!(engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await
            .is_none());
    }

    /// Once the token line is crossed the threshold path owns compaction, and
    /// the TTL pass stands down rather than compacting twice for one reason.
    #[tokio::test]
    async fn the_cache_ttl_pass_stands_down_at_the_token_line() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());

        // A context past the compaction line for this model.
        let auto = compact::CompactThresholds::resolve(&engine.model, &engine.compact_config).auto;
        turn.messages = vec![Message::user("x".repeat(auto * 4 + 4_000))];
        assert!(
            PromptCompiler::estimate_message_tokens(&turn.messages) >= auto,
            "the fixture must cross the compaction line"
        );

        // The cache went cold while the session sat idle.
        engine.last_request_at = Some(
            Instant::now()
                - std::time::Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );

        let outcome = engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await;
        assert!(
            matches!(
                outcome,
                Some(CompactOutcome::DeferredToThresholdPath { .. })
            ),
            "{outcome:?}"
        );
    }

    /// A cold cache below the token line compacts, and the pass is charged to
    /// nobody's recovery budget — tidying up must not starve a real recovery.
    #[tokio::test]
    async fn a_cold_cache_below_the_token_line_compacts_without_spending_a_retry() {
        let (mut engine, _rx, _session) = make_test_engine();
        let mut turn = TurnRuntime::new(TokenBudget::with_defaults());
        turn.messages = vec![Message::user("hello")];
        turn.recovery_count = 0;

        engine.last_request_at = Some(
            Instant::now()
                - std::time::Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );

        let outcome = engine
            .maybe_compact_on_cache_ttl(&mut turn, &SubmitOptions::default(), 1)
            .await;

        assert!(outcome.is_some(), "a cold cache should be acted on");
        assert_eq!(
            turn.recovery_count, 0,
            "a maintenance pass must not spend a recovery retry"
        );
    }

    /// The turn loop reaches the TTL pass: a cold cache is compacted before
    /// the request goes out, and the turn still completes.
    #[tokio::test]
    async fn the_turn_loop_compacts_a_cold_cache_before_the_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut engine, _rx, session) = make_test_engine();
        engine.set_transcript_path(tmp.path().to_path_buf());
        let transcript = tmp.path().join(format!("{}.jsonl", session.session_id));
        QueryEngine::write_transcript_replacement(&transcript, &[Message::user("earlier history")])
            .unwrap();

        engine.last_request_at = Some(
            Instant::now()
                - std::time::Duration::from_secs(engine.compact_config.cache_ttl_secs + 1),
        );

        let reason = engine
            .submit("go".into(), SubmitOptions::default())
            .await
            .unwrap();
        assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");

        // The pass ran: the transcript was rewritten, and a backup of what it
        // held before is on disk.
        let backups: Vec<PathBuf> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(".backup."))
            .collect();
        assert_eq!(backups.len(), 1, "the cold cache should have compacted");
    }
}
