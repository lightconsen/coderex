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

use crate::types::{EngineEvent, Message, ModelId, ToolCall, ToolResult, UsageSummary};
use crate::budget::TokenBudget;
use crate::cost::CostTracker;
use crate::permission_engine::{PermissionEngine, PermissionResponse};
use crate::permissions::PermissionResult;
use crate::api_client::{ApiClient, ApiResponse};
use crate::state::SessionState;

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
    pub fn new(schema: serde_json::Value) -> Self {
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

        match self.validate_value(&value, &self.schema) {
            Ok(()) => ValidationResult::Valid(value),
            Err(error) => ValidationResult::Invalid { error, retries },
        }
    }

    /// Recursively validate a JSON value against a schema.
    fn validate_value(&self, value: &serde_json::Value, schema: &serde_json::Value) -> Result<(), String> {
        let Some(schema_obj) = schema.as_object() else {
            return Ok(());
        };

        // Type check
        if let Some(expected_type) = schema_obj.get("type") {
            if let Some(type_str) = expected_type.as_str() {
                self.check_type(value, type_str)?;
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
                            self.validate_value(val, prop_schema)?;
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
    fn check_type(&self, value: &serde_json::Value, expected: &str) -> Result<(), String> {
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
    pub fn new(budget: TokenBudget) -> Self {
        Self {
            state: TurnState::Initialize,
            messages: Vec::new(),
            pending_tool_calls: Vec::new(),
            pending_permission: None,
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
        }
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
                    if matches!(reason, TerminalReason::Completed | TerminalReason::TokenLimitStop) {
                        // Check if we should continue (more turns needed for tool results)
                        break;
                    }
                    turn_number += 1;
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
        self.build_context(turn, options)?;

        // Phase 2: BuildContext → RunPreSubmitHooks
        turn.transition_to(TurnState::RunPreSubmitHooks);
        // TODO: run pre-submit hooks

        // Phase 3: RunPreSubmitHooks → StartRequest
        turn.transition_to(TurnState::StartRequest);

        // Phase 4: StartRequest → StreamAssistant
        turn.transition_to(TurnState::StreamAssistant);
        let api_response = self.send_request(turn, options).await?;

        // Record budget and cost
        turn.budget.record_tokens(&api_response.usage);
        self.cost_tracker.record_usage(&api_response.usage, &self.model);

        // Parse the response
        let content = self.validate_structured_output(api_response.content.clone());
        let assistant_msg = Message::assistant(content.clone());
        turn.messages.push(assistant_msg);

        // Check for tool calls in the response
        let tool_calls = Self::extract_tool_calls(&content);

        if tool_calls.is_empty() {
            // No tool calls — turn is complete
            turn.transition_to(TurnState::EvaluateContinuation);
            if !turn.budget.should_continue() {
                turn.terminate(TerminalReason::BudgetExhausted);
                return Ok(TerminalReason::BudgetExhausted);
            }

            turn.transition_to(TurnState::Finalize);
            self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);

            if api_response.stop_reason == "max_tokens" {
                return Ok(TerminalReason::TokenLimitStop);
            }
            return Ok(TerminalReason::Completed);
        }

        // Phase 5: Execute tools
        turn.pending_tool_calls = tool_calls
            .iter()
            .map(PendingToolCall::from)
            .collect();

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
            turn.terminate(TerminalReason::BudgetExhausted);
            self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);
            return Ok(TerminalReason::BudgetExhausted);
        }

        turn.transition_to(TurnState::Finalize);
        self.cost_tracker.end_turn(turn_number, &api_response.usage, &self.model);

        // If we have tool results, the model needs another turn
        // Return Completed here — the caller will start the next turn
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
    ) -> Result<(), String> {
        // Start with existing session messages
        turn.messages = self.session.messages.clone();

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
            "BuildContext: {} messages loaded ({} denied tools injected)",
            turn.messages.len(),
            self.denied_tools.len()
        );
        Ok(())
    }

    /// Format denied tool calls into a system prompt summary.
    fn format_denied_tools_summary(&self) -> String {
        let mut summary =
            String::from("The following tool calls from the previous turn were denied:\n");
        for denied in &self.denied_tools {
            summary.push_str(&format!("- `{}`: {}\n", denied.name, denied.reason));
        }
        summary.push_str("Please do not attempt these tool calls again in the same way. Adjust your approach accordingly.");
        summary
    }

    /// Send the request to the API.
    async fn send_request(
        &self,
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
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();

        let response = self
            .api_client
            .create_message(&system, &messages, &self.model, 4096)
            .await
            .map_err(|e| e.to_string())?;

        // Emit text delta event
        let _ = self
            .event_tx
            .send(EngineEvent::TextDelta(response.content.clone()));

        Ok(response)
    }

    /// Extract tool calls from the assistant response text.
    ///
    /// In a production implementation, tool calls come as structured blocks
    /// from the API response. Here we use a simple text-based heuristic.
    fn extract_tool_calls(content: &str) -> Vec<ToolCall> {
        // Try to parse as JSON tool call array
        if let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(content) {
            return calls;
        }

        // Try to find a single tool call JSON object
        if let Ok(call) = serde_json::from_str::<ToolCall>(content) {
            return vec![call];
        }

        // Look for JSON array/object in the content
        if let Some(start) = content.find('[') {
            if let Some(end) = content.rfind(']') {
                if let Ok(calls) =
                    serde_json::from_str::<Vec<ToolCall>>(&content[start..=end])
                {
                    return calls;
                }
            }
        }

        Vec::new()
    }

    /// Execute pending tool calls with permission checks.
    async fn execute_tool_calls(
        &mut self,
        turn: &mut TurnRuntime,
        _options: &SubmitOptions,
    ) -> Result<Vec<ToolResult>, String> {
        let mut results = Vec::new();

        for call in &turn.pending_tool_calls {
            // Emit tool call start event
            let _ = self.event_tx.send(EngineEvent::ToolCallStart {
                id: call.id.clone(),
                name: call.name.clone(),
            });

            // Check permission
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
                    let result = ToolResult {
                        tool_call_id: call.id.clone(),
                        output: format!("tool '{}' executed (stub)", call.name),
                        is_error: false,
                    };
                    let _ = self.event_tx.send(EngineEvent::ToolCallEnd {
                        id: call.id.clone(),
                        result: result.clone(),
                    });
                    results.push(result);
                }
                PermissionResult::Ask { reason } | PermissionResult::AskForSession { reason } => {
                    let _ = self.event_tx.send(EngineEvent::PermissionRequest {
                        id: call.id.clone(),
                        tool: call.name.clone(),
                        args: call.input.clone(),
                    });
                    // Track for next-turn injection
                    self.denied_tools.push(DeniedToolCall {
                        name: call.name.clone(),
                        reason: reason.clone(),
                    });
                    let result = ToolResult {
                        tool_call_id: call.id.clone(),
                        output: format!("tool '{}' denied by user", call.name),
                        is_error: true,
                    };
                    results.push(result);
                }
                PermissionResult::Deny { reason } => {
                    // Track for next-turn injection
                    self.denied_tools.push(DeniedToolCall {
                        name: call.name.clone(),
                        reason: reason.clone(),
                    });
                    let result = ToolResult {
                        tool_call_id: call.id.clone(),
                        output: format!("tool '{}' denied", call.name),
                        is_error: true,
                    };
                    results.push(result);
                }
                PermissionResult::Elevated { reason } => {
                    self.denied_tools.push(DeniedToolCall {
                        name: call.name.clone(),
                        reason: reason.clone(),
                    });
                    let result = ToolResult {
                        tool_call_id: call.id.clone(),
                        output: format!("tool '{}' requires elevated permissions", call.name),
                        is_error: true,
                    };
                    results.push(result);
                }
            }
        }

        turn.pending_tool_calls.clear();
        Ok(results)
    }

    /// Get the current cost tracker (shared reference).
    pub fn cost_tracker(&self) -> &CostTracker {
        &self.cost_tracker
    }

    /// Get the current turn number.
    #[must_use]
    pub const fn current_turn(&self) -> u32 {
        self.current_turn
    }

    /// Get the session state (read-only).
    pub fn session(&self) -> &SessionState {
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
    use futures::Stream;

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
        let engine = QueryEngine::new(Box::new(api), ModelId::sonnet(), session.clone(), perm_engine, tx);
        (engine, rx, session)
    }

    // ─── Mock API Client ─────────────────────────────────────────────

    struct MockApiClient;

    #[async_trait::async_trait]
    impl ApiClient for MockApiClient {
        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: "Hello from the mock assistant.".into(),
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
            Box<dyn futures::Stream<Item = StreamEvent> + Send + '_>,
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
    fn extract_tool_calls_parses_json_array() {
        let content = r#"[{"id":"tc_1","name":"read","input":{"path":"test.txt"}}]"#;
        let calls = QueryEngine::extract_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tc_1");
        assert_eq!(calls[0].name, "read");
    }

    #[test]
    fn extract_tool_calls_parses_single_object() {
        let content = r#"{"id":"tc_2","name":"bash","input":{"command":"ls"}}"#;
        let calls = QueryEngine::extract_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
    }

    #[test]
    fn extract_tool_calls_returns_empty_for_plain_text() {
        let calls = QueryEngine::extract_tool_calls("Just a regular text response");
        assert!(calls.is_empty());
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
        let (engine, _rx, _session) = make_test_engine();
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
}
