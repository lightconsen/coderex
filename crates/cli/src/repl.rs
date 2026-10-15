//! REPL loop — interactive conversation with the QueryEngine.

use crate::session_resume;
use anyhow::Result;
use coderex_agents::manager::AgentManager;
use coderex_commands::registry::{CommandContext, CommandRegistry, CommandResult};
use coderex_commands::{CommandHistory, MacroRegistry};
use coderex_core::api_client::ApiClient;
use coderex_core::confirmation_bus::{ConfirmationBus, ConfirmationRequest, ConfirmationResponse};
use coderex_core::elicitation::{ElicitationHandler, ElicitationResponse, ElicitationResult};
use coderex_core::engine::{QueryEngine, SubmitOptions, TerminalReason};
use coderex_core::orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, background_feature_label,
};
use coderex_core::permission_engine::{
    AutoClassifierHook, ExplainPermissionHook, PermissionEngine,
};
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::prompts;
use coderex_core::query_profiles::{ModelProfile, PromptProfile, QueryKind, QueryRequest};
use coderex_core::state::SessionState;
use coderex_core::types::ModelId;
use coderex_core::types::{EngineEvent, Message, ToolCall};
use coderex_memory::{
    DreamCheckResult, ai_workflow,
    auto_dream::{AutoDream, DreamConfig},
    long_term::LongTermStore,
    session_memory::SessionMemoryStore,
    team_memory::TeamMemorySyncService,
};
use coderex_sdk::{SdkEvent, SdkSession};
use coderex_tools::{
    AgentTool, ChromeClient, CronScheduler, ExecutionConfig, ExecutionContext, RateLimiter,
    RegistryToolDispatcher, TaskStore, ToolExecutor as ToolsExecutor, ToolRegistry,
    register_builtin_tools,
};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Session-level state that persists across turns.
#[derive(Debug, Default)]
struct SessionEffects {
    /// Active prompt profiles from mode commands.
    prompt_profiles: Vec<PromptProfile>,
    /// Additional system prompt text appended by commands.
    append_system_prompt: Option<String>,
    /// Full system prompt override (replaces base prompt).
    override_system_prompt: Option<String>,
    /// Allowed tools override (None = use defaults).
    allowed_tools_override: Option<Vec<String>>,
    /// Additional user-context fragments accumulated from commands.
    extra_context: Vec<String>,
    /// Whether a title has been generated for this session.
    title_generated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractivePhase {
    Idle,
    Planning,
    Thinking,
    RunningTool(String),
    WaitingForPermission(String),
}

impl InteractivePhase {
    fn reset(&mut self) {
        *self = Self::Idle;
    }

    fn note_text_delta(&mut self) {
        if !matches!(self, Self::RunningTool(_)) {
            *self = Self::Thinking;
        }
    }

    fn note_tool_start(&mut self, summary: &str) {
        *self = Self::RunningTool(summary.to_string());
    }

    fn note_tool_end(&mut self) {
        *self = Self::Thinking;
    }

    fn note_permission_request(&mut self, tool: &str, args: &serde_json::Value) {
        *self = Self::WaitingForPermission(permission_activity_label(
            &ToolCall::summarize_activity(tool, args),
        ));
    }

    fn status_label(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Planning => Some("Planning...".into()),
            Self::Thinking => Some("Thinking...".into()),
            Self::RunningTool(summary) | Self::WaitingForPermission(summary) => {
                Some(summary.clone())
            }
        }
    }
}

fn permission_activity_label(summary: &str) -> String {
    if summary.is_empty() {
        "Waiting for permission...".into()
    } else {
        format!("Waiting for permission: {summary}")
    }
}

fn normalize_plan_label(label: &str) -> String {
    let trimmed = label.trim();
    let trimmed = trimmed.trim_end_matches('.');
    let trimmed = trimmed.trim_end_matches("...");
    if trimmed.is_empty() {
        "Working".into()
    } else {
        trimmed.into()
    }
}

fn push_activity_row(ui_state: &mut coderex_ui::state::UiState, label: &str) {
    let concise = normalize_plan_label(label);
    let duplicate = ui_state
        .activity_log
        .last()
        .is_some_and(|last| last == &concise);
    if duplicate {
        return;
    }

    ui_state.activity_log.push(concise.clone());
    if ui_state.activity_log.len() > 25 {
        let excess = ui_state.activity_log.len() - 25;
        ui_state.activity_log.drain(..excess);
    }

    ui_state.messages.push(coderex_ui::message_list::Message {
        role: coderex_ui::message_list::MessageRole::System,
        content: concise.into(),
        tool_name: None,
        streaming: false,
        folded: false,
    });
}

fn set_plan_step(ui_state: &mut coderex_ui::state::UiState, label: &str) {
    let normalized = normalize_plan_label(label);
    if let Some(current) = ui_state.plan.steps.get_mut(ui_state.plan.current_step) {
        if current.label == normalized && !current.complete {
            ui_state.plan.current_label = Some(normalized);
            return;
        }
        current.complete = true;
    }

    ui_state.plan.steps.push(coderex_ui::state::PlanStep {
        label: normalized.clone(),
        complete: false,
    });
    ui_state.plan.current_step = ui_state.plan.steps.len().saturating_sub(1);
    ui_state.plan.current_label = Some(normalized);
}

fn complete_plan(ui_state: &mut coderex_ui::state::UiState) {
    if let Some(current) = ui_state.plan.steps.get_mut(ui_state.plan.current_step) {
        current.complete = true;
    }
}

fn selected_palette_command(ui_state: &coderex_ui::state::UiState) -> Option<String> {
    let coderex_ui::overlay::Overlay::CommandPalette {
        commands, selected, ..
    } = ui_state.overlays.top()?
    else {
        return None;
    };

    commands
        .get(*selected)
        .and_then(|entry| entry.split_whitespace().next())
        .map(ToString::to_string)
}

fn show_detail_overlay(
    ui_state: &mut coderex_ui::state::UiState,
    title: impl Into<String>,
    body: impl Into<String>,
) {
    ui_state
        .overlays
        .push(coderex_ui::overlay::Overlay::Detail {
            title: title.into(),
            body: body.into(),
        });
}

fn permission_request_args(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::Value::String(arguments.into()))
}

fn sync_permission_overlay(
    ui_state: &mut coderex_ui::state::UiState,
    tool: &str,
    args: &str,
    risk_summary: &str,
    always_option: bool,
) {
    ui_state.permission_detail = Some(coderex_ui::state::PermissionDetail {
        tool: tool.to_string(),
        args: args.to_string(),
        risk_summary: risk_summary.to_string(),
    });

    if matches!(
        ui_state.overlays.top(),
        Some(coderex_ui::overlay::Overlay::PermissionDialog { .. })
    ) {
        ui_state.overlays.pop();
    }

    ui_state
        .overlays
        .push(coderex_ui::overlay::Overlay::PermissionDialog {
            tool: tool.to_string(),
            args: args.to_string(),
            risk_summary: risk_summary.to_string(),
            always_option,
        });
}

fn clear_permission_overlay(ui_state: &mut coderex_ui::state::UiState) {
    ui_state.permission_detail = None;
    ui_state.permission_bar.clear_pending();
    if matches!(
        ui_state.overlays.top(),
        Some(coderex_ui::overlay::Overlay::PermissionDialog { .. })
    ) {
        ui_state.overlays.pop();
    }
}

fn core_message_to_ui_message(
    message: &coderex_core::types::Message,
) -> coderex_ui::message_list::Message {
    let (role, content, tool_name) = match (&message.role, &message.content) {
        (coderex_core::types::Role::User, coderex_core::types::MessageContent::Text(text)) => (
            coderex_ui::message_list::MessageRole::User,
            text.clone(),
            None,
        ),
        (coderex_core::types::Role::Assistant, coderex_core::types::MessageContent::Text(text)) => {
            (
                coderex_ui::message_list::MessageRole::Assistant,
                text.clone(),
                None,
            )
        }
        (coderex_core::types::Role::System, coderex_core::types::MessageContent::Text(text)) => (
            coderex_ui::message_list::MessageRole::System,
            text.clone(),
            None,
        ),
        (coderex_core::types::Role::Tool, coderex_core::types::MessageContent::ToolCall(call)) => (
            coderex_ui::message_list::MessageRole::ToolCall,
            call.activity_summary(),
            Some(call.name.clone()),
        ),
        (
            coderex_core::types::Role::Tool,
            coderex_core::types::MessageContent::ToolResult(result),
        ) => (
            coderex_ui::message_list::MessageRole::ToolResult,
            result.output.clone(),
            Some(result.tool_call_id.clone()),
        ),
        (_, content) => (
            coderex_ui::message_list::MessageRole::System,
            format!("{content:?}"),
            None,
        ),
    };

    coderex_ui::message_list::Message {
        role,
        content: content.into(),
        tool_name: tool_name.map(Into::into),
        streaming: false,
        folded: false,
    }
}

fn restore_transcript_messages(
    ui_state: &mut coderex_ui::state::UiState,
    messages: &[coderex_core::types::Message],
) {
    ui_state.messages = coderex_ui::message_list::MessageView::new(1000);
    ui_state.messages.start = 0;
    for message in messages {
        ui_state.messages.push(core_message_to_ui_message(message));
    }
}

fn apply_interactive_snapshot(
    ui_state: &mut coderex_ui::state::UiState,
    snapshot: &session_resume::InteractiveResumeSnapshot,
) {
    ui_state.plan.steps = snapshot
        .plan
        .steps
        .iter()
        .map(|step| coderex_ui::state::PlanStep {
            label: step.label.clone(),
            complete: step.complete,
        })
        .collect();
    ui_state.plan.current_step = snapshot.plan.current_step;
    ui_state.plan.current_label = snapshot.plan.current_label.clone();
    ui_state.activity_log = snapshot.activity_log.clone();
}

fn build_interactive_snapshot(
    ui_state: &coderex_ui::state::UiState,
    session_title: Option<String>,
) -> session_resume::InteractiveResumeSnapshot {
    session_resume::InteractiveResumeSnapshot {
        session_title,
        plan: session_resume::PersistedInteractivePlan {
            steps: ui_state
                .plan
                .steps
                .iter()
                .map(|step| session_resume::PersistedPlanStep {
                    label: step.label.clone(),
                    complete: step.complete,
                })
                .collect(),
            current_step: ui_state.plan.current_step,
            current_label: ui_state.plan.current_label.clone(),
        },
        activity_log: ui_state.activity_log.clone(),
    }
}

/// Result from a completed background task, delivered to the UI.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BackgroundTaskResult {
    /// The task ID.
    id: String,
    /// Which background feature ran.
    kind: BackgroundFeatureKind,
    /// Whether it succeeded.
    success: bool,
    /// Output content (result text or error message).
    content: String,
}

/// A clarification request emitted from the engine into the TUI.
#[derive(Debug)]
struct TuiElicitationRequest {
    /// The question text already shown by the assistant.
    prompt: String,
    /// Channel to send the user's answer back to the engine.
    response_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// Elicitation handler for the full-screen TUI.
///
/// This keeps clarification prompts inside the TUI state instead of writing
/// directly to stderr and corrupting the screen.
#[derive(Debug, Clone)]
struct TuiElicitationHandler {
    tx: tokio::sync::mpsc::Sender<TuiElicitationRequest>,
}

impl TuiElicitationHandler {
    fn new(tx: tokio::sync::mpsc::Sender<TuiElicitationRequest>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl ElicitationHandler for TuiElicitationHandler {
    async fn request(
        &self,
        questions: Vec<coderex_core::elicitation::ElicitationQuestion>,
    ) -> ElicitationResult {
        let Some(question) = questions.first() else {
            return ElicitationResult::Cancelled;
        };

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = TuiElicitationRequest {
            prompt: question.question.clone(),
            response_tx,
        };

        if self.tx.send(request).await.is_err() {
            return ElicitationResult::Cancelled;
        }

        match response_rx.await {
            Ok(value) => {
                let answer = value
                    .get("answer")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(ToString::to_string);

                if let Some(other_text) = answer {
                    ElicitationResult::Responded(ElicitationResponse {
                        request_id: question.header.clone(),
                        selected: Vec::new(),
                        other_text: Some(other_text),
                    })
                } else {
                    ElicitationResult::Cancelled
                }
            }
            Err(_) => ElicitationResult::Cancelled,
        }
    }
}

/// Handle for an active background task.
#[derive(Debug)]
struct BackgroundTask {
    /// Unique task ID.
    id: String,
    /// Feature kind.
    kind: BackgroundFeatureKind,
    /// Cancellation token — signal to abort the task.
    cancel_token: CancellationToken,
}

/// The REPL event loop — reads user input, executes commands or queries.
#[allow(missing_debug_implementations)]
pub struct ReplLoop {
    /// Command registry.
    commands: CommandRegistry,
    /// Current model.
    model: ModelId,
    /// Current working directory.
    cwd: String,
    /// Whether the loop should continue.
    running: bool,
    /// Prompt history.
    history: Vec<String>,
    /// QueryEngine for processing user queries (14.1 wired).
    engine: Option<QueryEngine>,
    /// Event receiver for streaming output.
    event_rx: Option<mpsc::UnboundedReceiver<EngineEvent>>,
    /// Session-level effects from commands.
    session_effects: SessionEffects,
    /// API client for orchestrator side-queries (title, permission explain, etc.).
    api_client: Option<Box<dyn ApiClient>>,
    /// Orchestrator for routing AI calls through product-level entry points.
    orchestrator: Option<coderex_core::orchestrator::Orchestrator>,
    /// Active background tasks (observable + cancelable).
    background_tasks: Vec<BackgroundTask>,
    /// Sender for background task results back to the UI thread.
    background_results_tx: Option<mpsc::UnboundedSender<BackgroundTaskResult>>,
    /// Receiver for background task results.
    background_results_rx: Option<mpsc::UnboundedReceiver<BackgroundTaskResult>>,
    /// Session memory store for persisting session facts.
    session_memory_store: Option<Arc<SessionMemoryStore>>,
    /// Long-term memory store for extracted memories.
    long_term_store: Option<Arc<LongTermStore>>,
    /// Session ID for memory operations.
    session_id: Option<uuid::Uuid>,
    /// Auto-dream manager for background session consolidation.
    auto_dream: Option<Arc<AutoDream>>,
    /// Shutdown signal for the auto-dream watcher.
    dream_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Team memory sync service for cross-repo memory sharing.
    team_memory_service: Option<Arc<TeamMemorySyncService>>,
    /// Config watcher for hot-reloading settings.
    #[cfg(feature = "hot-reload")]
    config_changed_rx: Option<tokio::sync::watch::Receiver<()>>,
    /// Command history with persistence.
    command_history: CommandHistory,
    /// Macro registry for user-defined command sequences.
    macro_registry: MacroRegistry,
    /// Pending destructive command awaiting confirmation.
    pending_destructive_cmd: Option<(String, Vec<String>)>,
    /// Plugin manager for dynamic plugin commands and tools.
    plugin_manager: Option<Arc<coderex_plugins::plugins::PluginManager>>,
    /// Primary tool executor (stored so plugin fallback can be layered on top).
    _primary_tool_executor:
        Option<Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync>>,
    /// Dispatcher reference for setting per-turn state (e.g., elicitation channel).
    dispatcher: Option<Arc<RegistryToolDispatcher>>,
    /// Sub-agent lifecycle manager shared with the registered Agent tool.
    agent_manager: Option<Arc<tokio::sync::Mutex<AgentManager>>>,
    /// The live MCP servers, filled in by `with_mcp_manager`.
    ///
    /// A slot rather than a plain field because the sub-agent tool factory is
    /// built before the MCP servers are attached, and reads this when it later
    /// builds a child registry.
    mcp_manager: Arc<std::sync::RwLock<Option<Arc<coderex_mcp::manager::McpManager>>>>,
    /// The server artifacts are published to, kept alive for the session.
    ///
    /// Dropping it would leave the model publishing pages nobody can open.
    artifact_server: Option<coderex_artifacts::ArtifactServer>,
    /// Where artifacts are stored, when it is not the default location.
    artifact_store_root: Option<std::path::PathBuf>,
    /// Pending elicitation response sender — when set, user input is treated as an answer.
    pending_elicitation: Option<tokio::sync::oneshot::Sender<serde_json::Value>>,
    /// Confirmation bus for interactive permission requests.
    confirmation_bus: Option<ConfirmationBus>,
    /// Receiver for cron job fire events.
    cron_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, String)>>,
    /// SDK session for headless/structured event consumers.
    sdk_session: Option<Arc<SdkSession>>,
    /// Background timeout checker for the SDK session.
    _sdk_timeout_handle: Option<tokio::task::JoinHandle<()>>,
    /// Teleport manager for session sharing and directory switching.
    teleport_manager: Option<coderex_bridge::teleport::TeleportManager>,
    /// Skip permission prompts (--dangerously-skip-permissions).
    skip_permissions: bool,
    /// Permission mode override (--permissions).
    permission_mode: Option<PermissionMode>,
    /// Disable session persistence (--no-session-persistence).
    no_persistence: bool,
    /// Disable slash commands (--disable-slash-commands).
    no_slash_commands: bool,
    /// JSON schema for structured output (--json-schema).
    json_schema: Option<String>,
    /// Maximum budget in USD for this session (--max-budget-usd).
    max_budget_usd: Option<f64>,
    /// Maximum number of turns (--max-turns).
    max_turns: Option<u32>,
    /// Replay user messages from previous session (--replay-user-messages).
    replay_user_messages: bool,
    /// Include partial messages in context (--include-partial-messages).
    include_partial_messages: bool,
    /// Beta features enabled (--betas).
    betas: Vec<String>,
    /// Chrome integration enabled (--chrome / --no-chrome).
    chrome_enabled: bool,
    /// Agent to use (--agent).
    agent: Option<String>,
    /// IDE mode (--ide).
    ide: Option<String>,
    /// Worktree mode (--worktree).
    worktree: bool,
    /// Fork from session ID (--fork-session).
    fork_session: Option<String>,
    /// Load context from PR URL (--from-pr).
    from_pr: Option<String>,
    /// Session name for transcript naming (--name).
    session_name: Option<String>,
    /// Additional directories to include in context (--add-dir).
    add_dir: Vec<String>,
    /// Input format for non-interactive mode (--input-format).
    input_format: String,
    /// Tmux integration (--tmux).
    tmux_enabled: bool,
    /// Debug file path (--debug-file).
    debug_file: Option<std::path::PathBuf>,
    /// When the current query started (for elapsed-time display).
    query_start_time: Option<std::time::Instant>,
    /// Whether Vim keybindings are enabled for interactive input.
    vim_mode_enabled: bool,
    /// Active TUI theme name.
    current_theme: String,
    /// Session id to resume into the current interactive loop.
    resume_session: Option<String>,
}

/// Wrapper that implements `ToolExecutor` for `Arc<dyn ToolExecutor>`.
///
/// Needed because `Arc<dyn Trait>` does not automatically implement `Trait`.
#[derive(Clone)]
struct ArcToolExecutor(Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync>);

#[async_trait::async_trait]
impl coderex_core::tool_executor::ToolExecutor for ArcToolExecutor {
    fn list_tools(&self) -> Vec<coderex_core::tool_executor::ToolInfo> {
        self.0.list_tools()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> coderex_core::tool_executor::ToolDispatchOutcome {
        self.0.execute(tool_call_id, tool_name, input).await
    }
}

/// Tool executor that falls back to plugin tools when the primary reports "not found".
#[derive(Clone)]
struct PluginToolFallbackExecutor {
    primary: ArcToolExecutor,
    plugin: Arc<coderex_plugins::tool_executor::PluginToolExecutor>,
}

#[async_trait::async_trait]
impl coderex_core::tool_executor::ToolExecutor for PluginToolFallbackExecutor {
    fn list_tools(&self) -> Vec<coderex_core::tool_executor::ToolInfo> {
        self.primary.list_tools()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> coderex_core::tool_executor::ToolDispatchOutcome {
        let primary = self.primary.execute(tool_call_id, tool_name, input).await;

        if primary.result.is_error && primary_tool_was_missing(&primary.result.output, tool_name) {
            self.plugin.execute(tool_call_id, tool_name, input).await
        } else {
            primary
        }
    }
}

fn primary_tool_was_missing(output: &str, tool_name: &str) -> bool {
    output == format!("tool '{tool_name}' not found in registry")
        || output.starts_with(&format!(
            "tool '{tool_name}' is not registered in this engine;"
        ))
}

impl ReplLoop {
    /// Create a new REPL loop without a QueryEngine.
    /// Use `with_engine` to wire up the QueryEngine for full conversation support.
    #[must_use]
    pub fn new(cwd: String, model: ModelId) -> Self {
        let mut commands = CommandRegistry::new();
        coderex_commands::registry::register_all_commands(&mut commands);
        let mut this = Self {
            commands,
            model,
            cwd,
            running: true,
            history: Vec::new(),
            engine: None,
            event_rx: None,
            session_effects: SessionEffects::default(),
            api_client: None,
            orchestrator: None,
            background_tasks: Vec::new(),
            background_results_tx: None,
            background_results_rx: None,
            session_memory_store: None,
            long_term_store: None,
            session_id: None,
            auto_dream: None,
            dream_shutdown_tx: None,
            team_memory_service: None,
            command_history: CommandHistory::new(1000),
            macro_registry: MacroRegistry::new(),
            pending_destructive_cmd: None,
            plugin_manager: None,
            _primary_tool_executor: None,
            dispatcher: None,
            agent_manager: None,
            mcp_manager: Arc::new(std::sync::RwLock::new(None)),
            artifact_server: None,
            artifact_store_root: None,
            pending_elicitation: None,
            confirmation_bus: None,
            cron_rx: None,
            sdk_session: None,
            _sdk_timeout_handle: None,
            teleport_manager: Some(coderex_bridge::teleport::TeleportManager::new()),
            #[cfg(feature = "hot-reload")]
            config_changed_rx: None,
            skip_permissions: false,
            permission_mode: None,
            no_persistence: false,
            no_slash_commands: false,
            json_schema: None,
            max_budget_usd: None,
            max_turns: None,
            replay_user_messages: false,
            include_partial_messages: false,
            betas: Vec::new(),
            chrome_enabled: true,
            agent: None,
            ide: None,
            worktree: false,
            fork_session: None,
            from_pr: None,
            session_name: None,
            add_dir: Vec::new(),
            input_format: "text".into(),
            tmux_enabled: false,
            debug_file: None,
            query_start_time: None,
            vim_mode_enabled: false,
            current_theme: "dark".into(),
            resume_session: None,
        };
        this.init_command_persistence();
        this
    }

    /// Create a new REPL loop wired to a QueryEngine (14.1).
    ///
    /// # Arguments
    /// * `api_client` — API client implementation for LLM requests.
    /// * `cwd` — Current working directory.
    /// * `model` — Model to use.
    #[must_use]
    pub fn with_engine(api_client: Box<dyn ApiClient>, cwd: String, model: ModelId) -> Self {
        let mut commands = CommandRegistry::new();
        coderex_commands::registry::register_all_commands(&mut commands);

        let session = SessionState::new(std::path::PathBuf::from(&cwd));
        let session_id = session.session_id;
        let checker = PermissionChecker::new(PermissionMode::default());
        let mut permission_engine = PermissionEngine::new(checker);
        let confirmation_bus = ConfirmationBus::new(10);
        let perm_path = std::path::PathBuf::from(&cwd)
            .join(".coderex")
            .join("permissions.json");
        permission_engine.set_persist_path(perm_path);
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EngineEvent>();
        permission_engine.add_hook(Arc::new(ExplainPermissionHook::new_with_event_tx(
            api_client.clone_box(),
            event_tx.clone(),
        )));
        permission_engine.add_hook(Arc::new(AutoClassifierHook::new(api_client.clone_box())));

        let memory_base = std::path::PathBuf::from(&cwd)
            .join(".coderex")
            .join("memory");
        let session_memory_store = Arc::new(SessionMemoryStore::new(memory_base.join("session")));
        let long_term_store = Arc::new(LongTermStore::new(memory_base.join("long_term")));

        let auto_dream = Arc::new(AutoDream::new(memory_base.clone(), DreamConfig::default()));
        let team_memory_service = Arc::new(TeamMemorySyncService::new(
            "default".to_string(),
            memory_base.join("team"),
        ));

        let engine = QueryEngine::new(
            api_client.clone_box(),
            model.clone(),
            session,
            permission_engine,
            event_tx.clone(),
        );

        let mut engine = engine;
        // Persistent session memory that survives compaction.
        engine.set_session_memory(Arc::new(
            coderex_memory::session_memory_backend::StoreSessionMemoryBackend::new(Arc::clone(
                &session_memory_store,
            )),
        ));

        let mut agent_manager = AgentManager::standalone();
        agent_manager.set_api_client(api_client.clone_box());
        agent_manager.set_model(model.clone());
        agent_manager.set_event_sender(event_tx);
        agent_manager.set_permission_checker(engine.permission_checker());
        let mcp_slot: Arc<std::sync::RwLock<Option<Arc<coderex_mcp::manager::McpManager>>>> =
            Arc::new(std::sync::RwLock::new(None));
        let mcp_for_subagents = Arc::clone(&mcp_slot);
        agent_manager.set_tool_executor_factory(move |cwd, sandbox| {
            let mut registry = ToolRegistry::new();
            register_builtin_tools(&mut registry);
            if let Some(mcp) = mcp_for_subagents.read().unwrap().as_ref() {
                mcp.register_tools_into(&registry);
            }
            let executor = Arc::new(ToolsExecutor::new(ExecutionConfig::default()));
            let mut dispatcher = RegistryToolDispatcher::new(
                Arc::new(registry),
                executor,
                ExecutionContext::Normal,
                cwd,
            );
            dispatcher.set_sandbox_override(sandbox);
            dispatcher.set_rate_limiter(Arc::new(RateLimiter::new()));
            dispatcher.set_chrome_client(ChromeClient::new());
            // The child registry intentionally has no nested AgentManager. A
            // synchronous parent holds its manager while waiting, so nesting
            // through that same manager could deadlock; the built-in Agent
            // tool instead reports that nested delegation is unavailable.
            Box::new(dispatcher)
        });
        let agent_manager = Arc::new(tokio::sync::Mutex::new(agent_manager));

        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        registry.register(Arc::new(
            AgentTool::new()
                .with_manager(Arc::clone(&agent_manager))
                .with_session_id(session_id.to_string())
                .with_cwd(cwd.clone()),
        ));
        let registry = Arc::new(registry);
        let inner_executor = Arc::new(ToolsExecutor::new(ExecutionConfig::default()));
        let mut dispatcher = RegistryToolDispatcher::new(
            registry,
            inner_executor,
            ExecutionContext::Normal,
            std::path::PathBuf::from(&cwd),
        );

        // Shared task store with persistence
        let config_home = coderex_config::config_home();
        let task_store = Arc::new(TaskStore::new().with_storage(config_home.join("tasks.jsonl")));
        let _ = task_store.load();
        dispatcher.set_task_store(Arc::clone(&task_store));

        // Shared cron scheduler with persistence
        let cron_scheduler =
            Arc::new(CronScheduler::new().with_storage(config_home.join("scheduled_tasks.json")));
        let _ = cron_scheduler.load();
        dispatcher.set_cron_scheduler(Arc::clone(&cron_scheduler));

        // Shared rate limiter for web tools
        let rate_limiter = Arc::new(RateLimiter::new());
        dispatcher.set_rate_limiter(Arc::clone(&rate_limiter));

        // Chrome DevTools client for browser automation tools
        dispatcher.set_chrome_client(ChromeClient::new());

        let dispatcher = Arc::new(dispatcher);
        let primary_executor: Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync> =
            dispatcher.clone();
        let arc_exec = ArcToolExecutor(Arc::clone(&primary_executor));
        engine.set_tool_executor(Box::new(arc_exec.clone()));
        engine.recover_orphaned_permissions();

        // Start cron background runner
        let cron_rx = cron_scheduler.spawn_runner();

        // Create SDK session and start its timeout checker
        let sdk_session = Arc::new(SdkSession::new(session_id.to_string()));
        let sdk_timeout_handle = sdk_session.spawn_timeout_checker();

        let orchestrator = coderex_core::orchestrator::Orchestrator::new(api_client.clone_box());
        let (bg_tx, bg_rx) = mpsc::unbounded_channel::<BackgroundTaskResult>();

        #[cfg(feature = "hot-reload")]
        let config_changed_rx = Self::init_config_watcher(&cwd);

        let mut this = Self {
            commands,
            model,
            cwd,
            running: true,
            history: Vec::new(),
            engine: Some(engine),
            event_rx: Some(event_rx),
            session_effects: SessionEffects::default(),
            api_client: Some(api_client),
            orchestrator: Some(orchestrator),
            background_tasks: Vec::new(),
            background_results_tx: Some(bg_tx),
            background_results_rx: Some(bg_rx),
            session_memory_store: Some(session_memory_store),
            long_term_store: Some(long_term_store),
            session_id: Some(session_id),
            auto_dream: Some(auto_dream),
            dream_shutdown_tx: None,
            team_memory_service: Some(team_memory_service),
            command_history: CommandHistory::new(1000),
            macro_registry: MacroRegistry::new(),
            pending_destructive_cmd: None,
            plugin_manager: None,
            _primary_tool_executor: Some(primary_executor),
            dispatcher: Some(dispatcher),
            agent_manager: Some(agent_manager),
            mcp_manager: mcp_slot,
            artifact_server: None,
            artifact_store_root: None,
            pending_elicitation: None,
            confirmation_bus: Some(confirmation_bus),
            cron_rx: Some(cron_rx),
            sdk_session: Some(sdk_session),
            _sdk_timeout_handle: sdk_timeout_handle,
            teleport_manager: Some(coderex_bridge::teleport::TeleportManager::new()),
            #[cfg(feature = "hot-reload")]
            config_changed_rx,
            skip_permissions: false,
            permission_mode: None,
            no_persistence: false,
            no_slash_commands: false,
            json_schema: None,
            max_budget_usd: None,
            max_turns: None,
            replay_user_messages: false,
            include_partial_messages: false,
            betas: Vec::new(),
            chrome_enabled: true,
            agent: None,
            ide: None,
            worktree: false,
            fork_session: None,
            from_pr: None,
            session_name: None,
            add_dir: Vec::new(),
            input_format: "text".into(),
            tmux_enabled: false,
            debug_file: None,
            query_start_time: None,
            vim_mode_enabled: false,
            current_theme: "dark".into(),
            resume_session: None,
        };
        this.init_command_persistence();
        this
    }

    /// Start serving artifacts and register the tool family.
    ///
    /// Failure is not fatal: a session that cannot publish still has every
    /// other tool, so the reason is logged and the family is left out.
    ///
    /// The tools land in the registry's dynamic tier — the same tier MCP tools
    /// use — because they arrive after the registry was shared. That keeps
    /// them out of plan mode, which is what we want: publishing is not a
    /// read-only planning action.
    pub async fn start_artifact_serving(&mut self) {
        let Some(dispatcher) = self.dispatcher.as_ref() else {
            return;
        };

        let store = coderex_artifacts::ArtifactStore::open(
            self.artifact_store_root
                .clone()
                .unwrap_or_else(coderex_artifacts::store::default_root),
        );

        // Prefer the well-known port so a URL stays valid across sessions; a
        // free one is better than no server when it is taken.
        let server = match coderex_artifacts::ArtifactServer::start(store.clone(), 8722).await {
            Ok(server) => server,
            Err(error) => {
                tracing::debug!("artifact port 8722 unavailable ({error}); taking a free one");
                match coderex_artifacts::ArtifactServer::start(store.clone(), 0).await {
                    Ok(server) => server,
                    Err(error) => {
                        tracing::warn!("artifact serving unavailable: {error}");
                        return;
                    }
                }
            }
        };

        let service = Arc::new(coderex_artifacts::ArtifactService::new(
            store,
            server.origin(),
            self.session_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "local".to_string()),
        ));

        for tool in coderex_artifacts::ArtifactTools::new(service).all() {
            dispatcher.registry().register_dynamic(tool);
        }
        tracing::info!(origin = server.origin(), "artifacts are served locally");

        self.artifact_server = Some(server);
    }

    /// Publish artifacts somewhere other than the default location.
    pub fn with_artifact_store_root(&mut self, root: std::path::PathBuf) {
        self.artifact_store_root = Some(root);
    }

    /// Wire the MCP manager into this REPL loop.
    ///
    /// Keeps the live server connections for the session and registers the
    /// tools they expose into the same registry as the built-in tools, so
    /// there is one execution path rather than a separate MCP one.
    ///
    /// # Panics
    ///
    /// Panics if the MCP slot's lock is poisoned.
    pub fn with_mcp_manager(&mut self, mcp_manager: Arc<coderex_mcp::manager::McpManager>) {
        if let Some(ref dispatcher) = self.dispatcher {
            mcp_manager.register_tools_into(dispatcher.registry());
        }
        *self.mcp_manager.write().expect("MCP slot poisoned") = Some(mcp_manager);
    }

    /// Wire a plugin manager into this REPL loop.
    ///
    /// Registers plugin-discovered slash commands and layers the plugin
    /// tool executor as a fallback behind the primary tool registry.
    pub fn with_plugin_manager(
        &mut self,
        plugin_manager: Arc<coderex_plugins::plugins::PluginManager>,
    ) {
        // Register plugin commands as /{plugin}/{command} slash commands
        coderex_plugins::command_bridge::PluginCommandBridge::register_commands(
            &plugin_manager,
            &mut self.commands,
        );

        // If we have a primary tool executor and an engine, layer plugin fallback
        if let Some(ref dispatcher) = self.dispatcher {
            if let Some(ref mut engine) = self.engine {
                let primary: Arc<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync> =
                    dispatcher.clone();
                let plugin_exec =
                    Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
                        Arc::clone(&plugin_manager),
                    ));
                let fallback = PluginToolFallbackExecutor {
                    primary: ArcToolExecutor(primary),
                    plugin: plugin_exec,
                };
                engine.set_tool_executor(Box::new(fallback));
            }
        }

        self.plugin_manager = Some(plugin_manager);
    }

    // ─── CLI Configuration Builder Methods ──────────────────────────────────

    pub fn with_skip_permissions(&mut self, skip: bool) {
        self.skip_permissions = skip;
    }

    pub fn with_permission_mode(&mut self, mode: Option<PermissionMode>) {
        self.permission_mode = mode;
    }

    pub fn with_no_persistence(&mut self, no_persist: bool) {
        self.no_persistence = no_persist;
    }

    pub fn with_no_slash_commands(&mut self, no_slash: bool) {
        self.no_slash_commands = no_slash;
    }

    pub fn with_json_schema(&mut self, schema: Option<String>) {
        self.json_schema = schema;
    }

    pub fn with_max_budget_usd(&mut self, budget: Option<f64>) {
        self.max_budget_usd = budget;
    }

    pub fn with_max_turns(&mut self, turns: Option<u32>) {
        self.max_turns = turns;
    }

    pub fn with_replay_user_messages(&mut self, replay: bool) {
        self.replay_user_messages = replay;
    }

    pub fn with_include_partial_messages(&mut self, include: bool) {
        self.include_partial_messages = include;
    }

    pub fn with_betas(&mut self, betas: Vec<String>) {
        self.betas = betas;
    }

    pub fn with_chrome_enabled(&mut self, enabled: bool) {
        self.chrome_enabled = enabled;
    }

    pub fn with_agent(&mut self, agent: Option<String>) {
        self.agent = agent;
    }

    pub fn with_ide(&mut self, ide: Option<String>) {
        self.ide = ide;
    }

    pub fn with_worktree(&mut self, worktree: bool) {
        self.worktree = worktree;
    }

    pub fn with_fork_session(&mut self, fork: Option<String>) {
        self.fork_session = fork;
    }

    pub fn with_resume_session(&mut self, session_id: Option<String>) {
        self.resume_session = session_id;
    }

    pub fn with_from_pr(&mut self, from_pr: Option<String>) {
        self.from_pr = from_pr;
    }

    pub fn with_session_name(&mut self, name: Option<String>) {
        self.session_name = name;
    }

    pub fn with_add_dir(&mut self, dirs: Vec<String>) {
        self.add_dir = dirs;
    }

    pub fn with_input_format(&mut self, format: String) {
        self.input_format = format;
    }

    pub fn with_tmux_enabled(&mut self, tmux: bool) {
        self.tmux_enabled = tmux;
    }

    pub fn with_debug_file(&mut self, path: Option<std::path::PathBuf>) {
        self.debug_file = path;
    }

    pub fn with_allowed_tools(&mut self, tools: Vec<String>) {
        self.session_effects.allowed_tools_override = Some(tools);
    }

    /// Apply all CLI configuration settings to the engine, dispatcher, and session.
    /// Must be called after `with_engine` (or `new`) and before `run_tui`/`run_headless`.
    pub fn apply_cli_config(&mut self) {
        // 1. Permission mode
        if self.skip_permissions {
            if let Some(ref mut engine) = self.engine {
                let checker = engine.permission_checker();
                let mut c = checker.lock().unwrap();
                c.set_mode(PermissionMode::BypassPermissions);
            }
        } else if let Some(mode) = self.permission_mode {
            if let Some(ref mut engine) = self.engine {
                let checker = engine.permission_checker();
                let mut c = checker.lock().unwrap();
                c.set_mode(mode);
            }
        }

        // 2. Disable slash commands
        if self.no_slash_commands {
            self.commands.clear();
        }

        // 2b. Disable session persistence — clear memory stores so nothing writes to disk
        if self.no_persistence {
            self.session_memory_store = None;
            self.long_term_store = None;
            self.auto_dream = None;
            self.team_memory_service = None;
            tracing::info!("Session persistence disabled — memory stores cleared");
        }

        // 3. Chrome integration
        if !self.chrome_enabled {
            if let Some(arc_dispatcher) = self.dispatcher.as_mut() {
                if let Some(dispatcher) = Arc::get_mut(arc_dispatcher) {
                    dispatcher.clear_chrome_client();
                }
            }
        }

        // 4. Session name / title
        if let Some(ref name) = self.session_name {
            if let Some(ref mut engine) = self.engine {
                let mut session = engine.session().clone();
                session.title = Some(name.clone());
                engine.set_session(session);
            }
        }

        // 5. Worktree mode
        if self.worktree {
            if let Some(ref mut engine) = self.engine {
                let mut session = engine.session().clone();
                if let Ok(base) = std::env::current_dir() {
                    let branch = std::process::Command::new("git")
                        .args(["rev-parse", "--abbrev-ref", "HEAD"])
                        .current_dir(&base)
                        .output();
                    let branch_str = branch
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| "unknown".into());
                    session.active_worktree = Some(coderex_core::state::WorktreeState {
                        path: base.clone(),
                        base: branch_str,
                        has_changes: false,
                    });
                    engine.set_session(session);
                }
            }
        }

        // 6. Resume or fork an existing transcript into the current engine session
        if let Some(session_id) = self.resume_session.clone() {
            if let Err(err) = self.load_session_state(&session_id, true) {
                tracing::warn!("Failed to restore resumed session {session_id}: {err}");
            }
        } else if let Some(fork_id) = self.fork_session.clone() {
            if let Err(err) = self.load_session_state(&fork_id, false) {
                tracing::warn!("Failed to load fork session {fork_id}: {err}");
            }
        }

        // 7. Replay user messages from most recent session
        if self.replay_user_messages {
            if let Some(ref mut engine) = self.engine {
                if let Some(session_id) = session_resume::latest_session_id(&self.cwd) {
                    if let Ok(msgs) = session_resume::load_resume_messages(&self.cwd, &session_id) {
                        let user_msgs: Vec<_> = msgs
                            .into_iter()
                            .filter(|m| matches!(m.role, coderex_core::types::Role::User))
                            .collect();
                        let mut session = engine.session().clone();
                        session.messages.extend(user_msgs);
                        engine.set_session(session);
                    }
                }
            }
        }

        // 8. Add directory contents to extra context
        for dir in &self.add_dir {
            let path = std::path::Path::new(dir);
            if path.exists() && path.is_dir() {
                let listing = Self::list_directory_contents(path);
                self.session_effects
                    .extra_context
                    .push(format!("Directory listing for {}:\n{}", dir, listing));
            }
        }

        // 9. PR context
        if let Some(ref pr_url) = self.from_pr {
            self.session_effects
                .extra_context
                .push(format!("Loading context from PR: {pr_url}"));
        }

        // 10. Agent context
        if let Some(ref agent) = self.agent {
            self.session_effects
                .extra_context
                .push(format!("Active agent: {agent}"));
        }

        // 11. IDE context
        if let Some(ref ide) = self.ide {
            self.session_effects
                .extra_context
                .push(format!("IDE mode: {ide}"));
        }

        // 12. Beta features
        if !self.betas.is_empty() {
            self.session_effects
                .extra_context
                .push(format!("Enabled beta features: {}", self.betas.join(", ")));
        }

        // 13. Tmux context
        if self.tmux_enabled {
            self.session_effects
                .extra_context
                .push("Tmux integration enabled".into());
        }

        // 14. Debug file — log that it would be configured
        if let Some(ref debug_path) = self.debug_file {
            tracing::info!("Debug output file configured: {debug_path:?}");
        }
    }

    /// List directory contents as a simple string.
    fn list_directory_contents(path: &std::path::Path) -> String {
        let mut entries = Vec::new();
        if let Ok(dir) = std::fs::read_dir(path) {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let meta = entry.metadata();
                let kind = meta
                    .as_ref()
                    .map_or("?", |m| if m.is_dir() { "d" } else { "f" });
                entries.push(format!("  [{kind}] {name}"));
            }
        }
        entries.join("\n")
    }

    fn current_session_id(&self) -> Option<String> {
        self.engine
            .as_ref()
            .map(|engine| engine.session().session_id.to_string())
    }

    fn current_session_title(&self) -> Option<String> {
        self.engine
            .as_ref()
            .and_then(|engine| engine.session().title.clone())
    }

    fn load_session_state(
        &mut self,
        session_id: &str,
        replace_session_id: bool,
    ) -> std::result::Result<
        (
            Vec<coderex_core::types::Message>,
            Option<session_resume::InteractiveResumeSnapshot>,
        ),
        String,
    > {
        let messages = session_resume::load_resume_messages(&self.cwd, session_id)
            .map_err(|e| e.to_string())?;
        let snapshot = session_resume::load_snapshot(&self.cwd, session_id);

        if let Some(ref mut engine) = self.engine {
            let mut session = engine.session().clone();
            if replace_session_id {
                if let Ok(parsed) = uuid::Uuid::parse_str(session_id) {
                    session.session_id = parsed;
                }
            }
            session.messages = messages.clone();
            if session.title.is_none() {
                session.title = snapshot
                    .as_ref()
                    .and_then(|state| state.session_title.clone());
            }
            engine.set_session(session);
        }

        self.history = messages
            .iter()
            .filter_map(|message| match (&message.role, &message.content) {
                (
                    coderex_core::types::Role::User,
                    coderex_core::types::MessageContent::Text(text),
                ) => Some(text.clone()),
                _ => None,
            })
            .collect();

        Ok((messages, snapshot))
    }

    fn persist_interactive_snapshot(&self, ui_state: &coderex_ui::state::UiState) {
        if self.no_persistence {
            return;
        }
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let session = engine.session();
        let session_id = session.session_id.to_string();
        let transcript_store = coderex_memory::transcript::TranscriptStore::new(
            session_resume::transcript_dir(&self.cwd),
        );
        if let Err(err) = transcript_store.rewrite(&session_id, &session.messages) {
            tracing::warn!("Failed to persist transcript for {session_id}: {err}");
        }
        let snapshot = build_interactive_snapshot(ui_state, self.current_session_title());
        if let Err(err) = session_resume::save_snapshot(&self.cwd, &session_id, &snapshot) {
            tracing::warn!("Failed to persist interactive snapshot for {session_id}: {err}");
        }
    }

    fn clear_interactive_snapshot(&self, session_id: Option<&str>) {
        if self.no_persistence {
            return;
        }
        let session_id = session_id
            .map(ToString::to_string)
            .or_else(|| self.current_session_id());
        if let Some(session_id) = session_id {
            if let Err(err) = session_resume::remove_snapshot(&self.cwd, &session_id) {
                tracing::warn!("Failed to remove interactive snapshot for {session_id}: {err}");
            }
        }
    }

    fn reset_restored_ui(&self, ui_state: &mut coderex_ui::state::UiState) {
        ui_state.overlays.clear();
        ui_state.notifications.dismiss_all();
        ui_state.input = coderex_ui::input::InputBuffer::new();
        ui_state.plan.reset();
        ui_state.activity_log.clear();
        ui_state.permission_bar.clear_status();
        clear_permission_overlay(ui_state);
        ui_state.layout_mode = coderex_ui::state::LayoutMode::SinglePane;
        ui_state.left_pane_content.clear();
        ui_state.left_pane_title.clear();
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking = false;
        ui_state.statusline.thinking_label = None;
    }

    fn restore_session_into_ui(
        &self,
        ui_state: &mut coderex_ui::state::UiState,
        messages: &[coderex_core::types::Message],
        snapshot: Option<&session_resume::InteractiveResumeSnapshot>,
    ) {
        self.reset_restored_ui(ui_state);
        restore_transcript_messages(ui_state, messages);
        if let Some(snapshot) = snapshot {
            apply_interactive_snapshot(ui_state, snapshot);
        }
    }

    fn resume_tui_session(
        &mut self,
        requested_session: Option<&str>,
        ui_state: &mut coderex_ui::state::UiState,
    ) -> std::result::Result<String, String> {
        let session_id = requested_session
            .map(ToString::to_string)
            .or_else(|| session_resume::latest_session_id(&self.cwd))
            .ok_or_else(|| {
                "No previous sessions found in .coderex/transcripts for this workspace.".to_string()
            })?;
        let (messages, snapshot) = self.load_session_state(&session_id, true)?;
        self.restore_session_into_ui(ui_state, &messages, snapshot.as_ref());

        let mut message = format!(
            "Resumed session: {session_id}\nLoaded {} messages from transcript.",
            messages.len()
        );
        if let Some(summary) = snapshot
            .as_ref()
            .and_then(|state| state.unfinished_context())
        {
            message.push_str("\n\n");
            message.push_str(&summary);
        }

        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::System,
            content: message.clone().into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
        ui_state.notifications.info(format!("Resumed {session_id}"));
        self.persist_interactive_snapshot(ui_state);
        Ok(message)
    }

    /// Forward an engine event into the SDK session's broadcast stream.
    fn forward_engine_event(&self, event: &EngineEvent) {
        if let Some(ref session) = self.sdk_session {
            session.emit(SdkEvent::Engine(event.clone()));
        }
    }

    /// Initialize command history and macro registry from disk.
    fn init_command_persistence(&mut self) {
        let config_dir = coderex_config::settings::config_home();
        let history_path = config_dir.join("command_history.jsonl");
        self.command_history.set_file_path(history_path.clone());
        if let Err(e) = self.command_history.load() {
            tracing::warn!("Failed to load command history: {e}");
        }

        let macros_path = config_dir.join("macros.json");
        self.macro_registry.set_file_path(macros_path);
        if let Err(e) = self.macro_registry.load() {
            tracing::warn!("Failed to load macros: {e}");
        }
    }

    /// Initialize the ConfigWatcher if the hot-reload feature is enabled.
    #[cfg(feature = "hot-reload")]
    fn init_config_watcher(cwd: &str) -> Option<tokio::sync::watch::Receiver<()>> {
        use coderex_config::ConfigWatcher;
        let user_path = coderex_config::config_home().join("settings.json");
        let project_path = std::path::Path::new(cwd).join(".coderex/settings.json");
        let mut paths = Vec::new();
        if user_path.exists() {
            paths.push(user_path);
        }
        if project_path.exists() {
            paths.push(project_path);
        }
        if paths.is_empty() {
            return None;
        }
        match ConfigWatcher::new(&paths) {
            Ok(watcher) => Some(watcher.subscribe()),
            Err(e) => {
                tracing::warn!("Failed to initialize config watcher: {e}");
                None
            }
        }
    }

    /// Run the interactive REPL loop reading from stdin.
    #[allow(clippy::unused_async)]
    pub async fn run_interactive(&mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();

        crate::welcome::print_welcome_screen(
            &mut stdout,
            env!("CARGO_PKG_VERSION"),
            &self.model.0,
            &self.cwd,
            self.engine.is_some(),
        )?;

        loop {
            if !self.running {
                break;
            }

            // Drain any cron fire events
            if let Some(ref mut rx) = self.cron_rx {
                while let Ok((job_id, prompt)) = rx.try_recv() {
                    writeln!(stdout, "[Cron {job_id}] {prompt}")?;
                }
            }

            write!(stdout, "❯ ")?;
            stdout.flush()?;

            let mut line = String::new();
            let bytes = stdin.lock().read_line(&mut line)?;
            if bytes == 0 {
                // EOF (Ctrl+D)
                break;
            }

            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }

            self.handle_input(line, &mut stdout).await?;
        }

        writeln!(stdout, "\nGoodbye!")?;
        Ok(())
    }

    /// Run the interactive REPL with full ratatui TUI.
    pub async fn run_tui(&mut self) -> Result<()> {
        use coderex_ui::input::{InputAction, InputBuffer};
        use coderex_ui::message_list::{Message, MessageRole};
        use coderex_ui::overlay::Overlay;
        use coderex_ui::state::{UiState, ViewMode};
        use coderex_ui::terminal::TerminalUi;
        use coderex_ui::vim::{VimAction, VimMode, VimState};
        use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        use std::time::{Duration, Instant};

        // Artifacts are published to a local origin, so the server has to be
        // up before the model can publish anything. It is started here rather
        // than where the registry is built because starting a server needs a
        // runtime; the tools are registered onto the live registry instead.
        self.start_artifact_serving().await;

        enable_raw_mode()?;
        let mut terminal_ui = TerminalUi::new()?;
        terminal_ui.enter()?;

        let mut ui_state = UiState::new();
        let mut confirmation_rx = if let Some(bus) = self.confirmation_bus.clone() {
            if let Some(engine) = self.engine.as_mut() {
                engine.set_confirmation_bus(bus.clone());
            }
            if let Some(manager) = self.agent_manager.as_ref() {
                manager.lock().await.set_confirmation_bus(bus.clone());
            }
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ConfirmationRequest>();
            tokio::spawn(async move {
                while let Some(request) = bus.next_request().await {
                    if tx.send(request).is_err() {
                        break;
                    }
                }
            });
            Some(rx)
        } else {
            None
        };

        if let Some(ref engine) = self.engine {
            if !engine.session().messages.is_empty() {
                let snapshot = session_resume::load_snapshot(
                    &self.cwd,
                    &engine.session().session_id.to_string(),
                );
                self.restore_session_into_ui(
                    &mut ui_state,
                    &engine.session().messages,
                    snapshot.as_ref(),
                );
                if self.resume_session.is_some() {
                    let mut note = format!(
                        "Resumed session: {}\nLoaded {} messages from transcript.",
                        engine.session().session_id,
                        engine.session().messages.len()
                    );
                    if let Some(summary) = snapshot.and_then(|state| state.unfinished_context()) {
                        note.push_str("\n\n");
                        note.push_str(&summary);
                    }
                    ui_state.messages.push(Message {
                        role: MessageRole::System,
                        content: note.into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                }
            } else {
                ui_state.messages.push(Message {
                    role: MessageRole::System,
                    content: format!(
                        "CodeRex v{} | model: {} | cwd: {} | Type /help for commands, Ctrl+C to clear, Ctrl+D to exit",
                        env!("CARGO_PKG_VERSION"),
                        self.model.0,
                        self.cwd
                    )
                    .into(),
                    tool_name: None,
                    streaming: false,
                    folded: false,
                });
            }
        } else {
            ui_state.messages.push(Message {
                role: MessageRole::System,
                content: format!(
                    "CodeRex v{} | model: {} | cwd: {} | Type /help for commands, Ctrl+C to clear, Ctrl+D to exit",
                    env!("CARGO_PKG_VERSION"),
                    self.model.0,
                    self.cwd
                )
                .into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        }

        // Crossterm event channel — read in a blocking thread
        let (ct_tx, mut ct_rx) = tokio::sync::mpsc::unbounded_channel::<crossterm::event::Event>();
        std::thread::spawn(move || {
            loop {
                match crossterm::event::poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        if let Ok(event) = crossterm::event::read() {
                            if ct_tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(false) => continue,
                    Err(_) => break,
                }
            }
        });

        let mut event_rx = self.event_rx.take();
        let mut vim = VimState::new_insert();
        let mut thinking = false;
        let mut phase = InteractivePhase::Idle;
        let mut query_done_rx: Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        > = None;
        let mut elicit_rx: Option<tokio::sync::mpsc::Receiver<coderex_tools::ElicitRequest>> = None;
        let mut tui_elicit_rx: Option<tokio::sync::mpsc::Receiver<TuiElicitationRequest>> = None;
        let mut queued_prompts = std::collections::VecDeque::<String>::new();
        let mut pending_confirmation: Option<ConfirmationRequest> = None;
        let mut last_stream_snapshot = Instant::now();

        let result = loop {
            if !self.running {
                break Ok(());
            }

            // Sync statusline from REPL state
            self.sync_statusline(&mut ui_state.statusline);
            ui_state.statusline.thinking = thinking;
            ui_state.statusline.plan_status = if thinking {
                ui_state.plan.status_label()
            } else {
                None
            };
            if thinking {
                ui_state.statusline.spinner_frame =
                    ui_state.statusline.spinner_frame.wrapping_add(1);
                ui_state.statusline.thinking_label =
                    phase.status_label().or_else(|| Some("Thinking...".into()));
                if ui_state.statusline.thinking_started.is_none() {
                    ui_state.statusline.thinking_started = Some(std::time::Instant::now());
                }
            } else {
                ui_state.statusline.thinking_label = None;
                ui_state.statusline.spinner_frame = 0;
                if ui_state.statusline.thinking_started.is_some() {
                    if let Some(start) = self.query_start_time {
                        ui_state.statusline.last_duration = Some(start.elapsed());
                    }
                    ui_state.statusline.thinking_started = None;
                }
            }
            // Sync permission bar
            self.sync_permission_bar(&mut ui_state.permission_bar);

            // Poll for completed background tasks and route by kind
            for result in self.poll_background_results() {
                match result.kind {
                    BackgroundFeatureKind::PromptSuggestion => {
                        if result.success {
                            ui_state.input.suggestion = Some(result.content.trim().to_string());
                        }
                    }
                    BackgroundFeatureKind::SessionMemoryUpdate => {
                        self.handle_memory_update_result(&result).await;
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: if result.success {
                                "Session memory updated.".into()
                            } else {
                                format!("Session memory update failed: {}", result.content).into()
                            },
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    BackgroundFeatureKind::MemoryExtraction => {
                        self.handle_memory_extraction_result(&result).await;
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: if result.success {
                                "Long-term memories extracted.".into()
                            } else {
                                format!("Memory extraction failed: {}", result.content).into()
                            },
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    _ => {
                        let label = format!(
                            "[Background {:?}] {}",
                            result.kind,
                            if result.success { "Done" } else { "Failed" }
                        );
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: format!("{label}\n{}", result.content).into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                }
            }

            // Draw frame
            if let Err(e) = terminal_ui.draw(&mut ui_state) {
                tracing::warn!("TUI draw error: {e}");
            }

            tokio::select! {
                Some(event) = ct_rx.recv() => {
                    match event {
                        Event::Key(key) => {
                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                if let Some(request) = pending_confirmation.take() {
                                    self.respond_to_confirmation(
                                        request,
                                        ConfirmationResponse::Denied,
                                        &mut ui_state,
                                        &mut phase,
                                    )
                                    .await;
                                    continue;
                                }
                                if thinking {
                                    thinking = false;
                                    phase.reset();
                                    query_done_rx = None;
                                    elicit_rx = None;
                                    tui_elicit_rx = None;
                                    self.pending_elicitation = None;
                                    clear_permission_overlay(&mut ui_state);
                                    if let Some(last) = ui_state
                                        .messages
                                        .messages
                                        .iter_mut()
                                        .rev()
                                        .find(|msg| msg.role == MessageRole::Assistant && msg.streaming)
                                    {
                                        last.streaming = false;
                                        let mut msg = last.content.to_string();
                                        msg.push_str(" (interrupted)");
                                        last.content = msg.into();
                                    }
                                    if let Some(ref dispatcher) = self.dispatcher {
                                        dispatcher.set_elicit_tx(None);
                                    }
                                    pending_confirmation = None;
                                    continue;
                                }
                                if !ui_state.input.text.is_empty() {
                                    ui_state.input = InputBuffer::new();
                                } else if ui_state.overlays.is_active() {
                                    ui_state.overlays.clear();
                                }
                                continue;
                            }

                            if key.code == KeyCode::Char('d')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                break Ok(());
                            }

                            // Shift+Tab: cycle permission mode
                            if key.code == KeyCode::BackTab {
                                ui_state.permission_bar.cycle_mode();
                                if let Some(ref mut engine) = self.engine {
                                    let checker = engine.permission_checker();
                                    if let Ok(mut c) = checker.lock() {
                                        c.set_mode(ui_state.permission_bar.mode);
                                    }
                                }
                                continue;
                            }

                            // Ctrl+Shift+S: toggle split-pane view
                            if key.code == KeyCode::Char('S')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                use coderex_ui::state::LayoutMode;
                                ui_state.layout_mode = match ui_state.layout_mode {
                                    LayoutMode::SinglePane => LayoutMode::HorizontalSplit { left_percent: 50 },
                                    LayoutMode::HorizontalSplit { .. } => LayoutMode::SinglePane,
                                };
                                continue;
                            }

                            // Handle Confirm overlay for destructive commands
                            if matches!(ui_state.overlays.top(), Some(coderex_ui::overlay::Overlay::Confirm { .. })) {
                                match key.code {
                                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                                        ui_state.overlays.pop();
                                        if let Some((cmd, args)) = self.pending_destructive_cmd.take() {
                                            self.execute_and_display_command(&cmd, &args, &mut ui_state).await;
                                        }
                                    }
                                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                        ui_state.overlays.pop();
                                        self.pending_destructive_cmd = None;
                                        ui_state.messages.push(coderex_ui::message_list::Message {
                                            role: coderex_ui::message_list::MessageRole::System,
                                            content: "Command cancelled.".into(),
                                            tool_name: None,
                                            streaming: false,
                                            folded: false,
                                        });
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            if matches!(
                                ui_state.overlays.top(),
                                Some(coderex_ui::overlay::Overlay::PermissionDialog { .. })
                            ) {
                                match key.code {
                                    KeyCode::Char('a') => {
                                        if let Some(request) = pending_confirmation.take() {
                                            self.respond_to_confirmation(
                                                request,
                                                ConfirmationResponse::Approved,
                                                &mut ui_state,
                                                &mut phase,
                                            )
                                            .await;
                                        }
                                    }
                                    KeyCode::Char('A') => {
                                        if let Some(request) = pending_confirmation.take() {
                                            self.respond_to_confirmation(
                                                request,
                                                ConfirmationResponse::ApproveSession,
                                                &mut ui_state,
                                                &mut phase,
                                            )
                                            .await;
                                        }
                                    }
                                    KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Esc => {
                                        if let Some(request) = pending_confirmation.take() {
                                            self.respond_to_confirmation(
                                                request,
                                                ConfirmationResponse::Denied,
                                                &mut ui_state,
                                                &mut phase,
                                            )
                                            .await;
                                        } else {
                                            clear_permission_overlay(&mut ui_state);
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }

                            let inline_palette_active = matches!(
                                ui_state.overlays.top(),
                                Some(coderex_ui::overlay::Overlay::CommandPalette { query, .. })
                                    if query.starts_with('/')
                            );
                            if inline_palette_active {
                                match key.code {
                                    KeyCode::Up => {
                                        ui_state.overlays.move_palette_selection(-1);
                                        continue;
                                    }
                                    KeyCode::Down => {
                                        ui_state.overlays.move_palette_selection(1);
                                        continue;
                                    }
                                    KeyCode::Tab => {
                                        if let Some(command) = selected_palette_command(&ui_state) {
                                            ui_state.input.text = format!("{command} ");
                                            ui_state.input.cursor = ui_state.input.text.len();
                                            self.update_slash_suggestions(&mut ui_state);
                                        }
                                        continue;
                                    }
                                    KeyCode::Esc if vim.is_insert() => {
                                        if matches!(
                                            ui_state.overlays.top(),
                                            Some(coderex_ui::overlay::Overlay::CommandPalette { .. })
                                        ) {
                                            ui_state.overlays.pop();
                                        }
                                        continue;
                                    }
                                    _ => {}
                                }
                            }

                            if !self.vim_mode_enabled && key.code == KeyCode::Esc {
                                if ui_state.overlays.is_active() {
                                    ui_state.overlays.clear();
                                }
                                ui_state.active_view = ViewMode::MainRepl;
                                continue;
                            }

                            let action = vim.handle_key(key);
                            match action {
                                VimAction::Submit => {
                                    if let Some(text) = ui_state.input.take() {
                                        self.process_tui_input(
                                            &text,
                                            &mut ui_state,
                                            &mut thinking,
                                            &mut phase,
                                            &mut query_done_rx,
                                            &mut elicit_rx,
                                            &mut tui_elicit_rx,
                                            &mut queued_prompts,
                                        ).await;
                                    }
                                }
                                VimAction::Escape => {
                                    if thinking {
                                        thinking = false;
                                        phase.reset();
                                        query_done_rx = None;
                                        elicit_rx = None;
                                        tui_elicit_rx = None;
                                        self.pending_elicitation = None;
                                        pending_confirmation = None;
                                        clear_permission_overlay(&mut ui_state);
                                        if let Some(last) = ui_state
                                            .messages
                                            .messages
                                            .iter_mut()
                                            .rev()
                                            .find(|msg| msg.role == MessageRole::Assistant && msg.streaming)
                                        {
                                            last.streaming = false;
                                            let mut msg = last.content.to_string();
                                            msg.push_str(" (cancelled)");
                                            last.content = msg.into();
                                        }
                                        if let Some(ref dispatcher) = self.dispatcher {
                                            dispatcher.set_elicit_tx(None);
                                        }
                                    } else if vim.mode == VimMode::Normal {
                                        ui_state.overlays.clear();
                                        ui_state.active_view = ViewMode::MainRepl;
                                    }
                                }
                                VimAction::Insert => {
                                    if vim.is_insert() {
                                        match key.code {
                                            KeyCode::Char(c) => {
                                                ui_state.input.apply(&InputAction::InsertChar(c));
                                            }
                                            KeyCode::Backspace => {
                                                ui_state.input.apply(&InputAction::Backspace);
                                            }
                                            KeyCode::Delete => {
                                                ui_state.input.apply(&InputAction::Delete);
                                            }
                                            KeyCode::Left => {
                                                ui_state.input.apply(&InputAction::CursorLeft);
                                            }
                                            KeyCode::Right => {
                                                ui_state.input.apply(&InputAction::CursorRight);
                                            }
                                            KeyCode::Up => {
                                                ui_state.input.apply(&InputAction::HistoryUp);
                                            }
                                            KeyCode::Down => {
                                                ui_state.input.apply(&InputAction::HistoryDown);
                                            }
                                            KeyCode::Home => {
                                                ui_state.input.apply(&InputAction::MoveToStart);
                                            }
                                            KeyCode::End => {
                                                ui_state.input.apply(&InputAction::MoveToEnd);
                                            }
                                            KeyCode::PageUp => {
                                                let page =
                                                    ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                                ui_state.messages.scroll_up(page);
                                            }
                                            KeyCode::PageDown => {
                                                let page =
                                                    ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                                ui_state.messages.scroll_down_by(page);
                                            }
                                            _ => {}
                                        }
                                    } else if self.vim_mode_enabled {
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::Left => {
                                    ui_state.input.apply(&InputAction::CursorLeft);
                                }
                                VimAction::Right => {
                                    ui_state.input.apply(&InputAction::CursorRight);
                                }
                                VimAction::Up => {
                                    ui_state.input.apply(&InputAction::HistoryUp);
                                }
                                VimAction::Down => {
                                    ui_state.input.apply(&InputAction::HistoryDown);
                                }
                                VimAction::JumpToStart => {
                                    ui_state.input.apply(&InputAction::MoveToStart);
                                }
                                VimAction::JumpToEnd => {
                                    ui_state.input.apply(&InputAction::MoveToEnd);
                                }
                                VimAction::DeleteToEnd => {
                                    ui_state.input.apply(&InputAction::KillToEnd);
                                }
                                VimAction::DeleteLine => {
                                    ui_state.input.text.clear();
                                    ui_state.input.cursor = 0;
                                }
                                VimAction::DeleteWord => {
                                    ui_state.input.apply(&InputAction::DeleteWord);
                                }
                                VimAction::DeleteChar => {
                                    ui_state.input.apply(&InputAction::Delete);
                                }
                                VimAction::YankLine => {
                                    vim.yank(&ui_state.input.text);
                                }
                                VimAction::Paste => {
                                    let yanked = vim.yanked().to_string();
                                    for c in yanked.chars() {
                                        ui_state.input.apply(&InputAction::InsertChar(c));
                                    }
                                }
                                VimAction::Undo => {
                                    if let Some(prev) = vim.pop_undo() {
                                        vim.push_undo(&ui_state.input.text);
                                        ui_state.input.text = prev;
                                        ui_state.input.cursor = ui_state.input.text.len();
                                    }
                                }
                                VimAction::PageUp => {
                                    let page =
                                        ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                    ui_state.messages.scroll_up(page);
                                }
                                VimAction::PageDown => {
                                    let page =
                                        ui_state.messages.viewport_height.saturating_sub(2).max(1);
                                    ui_state.messages.scroll_down_by(page);
                                }
                                VimAction::HalfPageUp => {
                                    let half_page =
                                        (ui_state.messages.viewport_height / 2).max(1);
                                    ui_state.messages.scroll_up(half_page);
                                }
                                VimAction::HalfPageDown => {
                                    let half_page =
                                        (ui_state.messages.viewport_height / 2).max(1);
                                    ui_state.messages.scroll_down_by(half_page);
                                }
                                VimAction::JumpTop => {
                                    ui_state.messages.start = 0;
                                    ui_state.messages.scroll_offset = usize::MAX;
                                }
                                VimAction::JumpBottom => {
                                    ui_state.messages.scroll_to_bottom();
                                }
                                VimAction::Command => {
                                    let cmds: Vec<String> = self
                                        .commands
                                        .list()
                                        .iter()
                                        .map(|c| format!("/{} - {}", c.name, c.description))
                                        .collect();
                                    ui_state.overlays.push(Overlay::CommandPalette {
                                        query: ":".into(),
                                        commands: cmds,
                                        selected: 0,
                                    });
                                }
                                VimAction::Search => {
                                    ui_state.active_view = ViewMode::Search;
                                }
                                VimAction::ToggleFold => {
                                    // Toggle fold on the last visible message (or focused if selector active)
                                    if let Some(msg) = ui_state.messages.messages.last_mut() {
                                        msg.folded = !msg.folded;
                                    }
                                }
                                VimAction::ToggleSplit => {
                                    use coderex_ui::state::LayoutMode;
                                    ui_state.layout_mode = match ui_state.layout_mode {
                                        LayoutMode::SinglePane => LayoutMode::HorizontalSplit { left_percent: 50 },
                                        LayoutMode::HorizontalSplit { .. } => LayoutMode::SinglePane,
                                    };
                                }
                                VimAction::SearchNext => {
                                    ui_state.search.next();
                                }
                                VimAction::SearchPrev => {
                                    ui_state.search.prev();
                                }
                                VimAction::ExportCopy => {
                                    use coderex_ui::export::{ExportFormat, export_messages, copy_to_clipboard};
                                    use coderex_ui::selector::MessageSelector;
                                    let core_msgs: Vec<coderex_core::types::Message> = ui_state.messages.messages.iter().map(|m| {
                                        let role = match m.role {
                                            coderex_ui::message_list::MessageRole::User => coderex_core::types::Role::User,
                                            coderex_ui::message_list::MessageRole::Assistant => coderex_core::types::Role::Assistant,
                                            coderex_ui::message_list::MessageRole::System => coderex_core::types::Role::System,
                                            coderex_ui::message_list::MessageRole::ToolCall | coderex_ui::message_list::MessageRole::ToolResult => coderex_core::types::Role::Tool,
                                        };
                                        coderex_core::types::Message {
                                            role,
                                            content: coderex_core::types::MessageContent::Text(m.content.to_string()),
                                        }
                                    }).collect();
                                    let selector = MessageSelector::new(core_msgs.len());
                                    let exported = export_messages(&core_msgs, &selector, ExportFormat::PlainText);
                                    if let Err(e) = copy_to_clipboard(&exported) {
                                        ui_state.notifications.err(format!("Copy failed: {e}"));
                                    } else {
                                        ui_state.notifications.success("Copied to clipboard");
                                    }
                                }
                                VimAction::Repeat
                                | VimAction::Ignore
                                => {}
                                VimAction::Append => {
                                    if self.vim_mode_enabled {
                                        if ui_state.input.cursor < ui_state.input.text.len() {
                                            ui_state.input.cursor += 1;
                                        }
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::InsertAtEnd => {
                                    if self.vim_mode_enabled {
                                        ui_state.input.cursor = ui_state.input.text.len();
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                                VimAction::InsertAtStart => {
                                    if self.vim_mode_enabled {
                                        ui_state.input.cursor = 0;
                                        vim.mode = VimMode::Insert;
                                    }
                                }
                            }

                            // Sync slash-command suggestion overlay when typing in insert mode.
                            if vim.is_insert() {
                                self.update_slash_suggestions(&mut ui_state);
                            }
                        }
                        Event::Mouse(mouse) => {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => ui_state.messages.scroll_up(3),
                                MouseEventKind::ScrollDown => ui_state.messages.scroll_down_by(3),
                                _ => {}
                            }
                        }
                        Event::Resize(_, _) => {}
                        _ => {}
                    }
                }

                _ = tokio::time::sleep(Duration::from_millis(100)), if thinking => {}

                Some(engine_event) = async {
                    event_rx.as_mut().unwrap().recv().await
                }, if event_rx.is_some() => {
                    self.forward_engine_event(&engine_event);
                    match engine_event {
                        EngineEvent::TextDelta(delta) => {
                            phase.note_text_delta();
                            set_plan_step(&mut ui_state, "Thinking through response");
                            if let Some(last) = ui_state
                                .messages
                                .messages
                                .iter_mut()
                                .rev()
                                .find(|msg| msg.role == MessageRole::Assistant)
                            {
                                let mut content = last.content.to_string();
                                content.push_str(&delta);
                                last.content = content.into();
                                last.streaming = true;
                            }
                            if last_stream_snapshot.elapsed() >= Duration::from_secs(1) {
                                self.persist_interactive_snapshot(&ui_state);
                                last_stream_snapshot = Instant::now();
                            }
                        }
                        EngineEvent::ToolCallStart { name, summary, .. } => {
                            phase.note_tool_start(&summary);
                            set_plan_step(&mut ui_state, &summary);
                            push_activity_row(&mut ui_state, &summary);
                            ui_state.messages.push(Message {
                                role: MessageRole::ToolCall,
                                content: summary.into(),
                                tool_name: Some(name.into()),
                                streaming: false,
                                folded: false,
                            });
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        EngineEvent::ToolCallEnd { result, .. } => {
                            phase.note_tool_end();
                            complete_plan(&mut ui_state);
                            pending_confirmation = None;
                            clear_permission_overlay(&mut ui_state);
                            let content = if result.is_error {
                                ui_state.notifications.err("Tool failed");
                                format!("Error: {}", result.output)
                            } else {
                                ui_state.notifications.success("Tool completed");
                                result.output
                            };
                            ui_state.messages.push(Message {
                                role: MessageRole::ToolResult,
                                content: content.into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        EngineEvent::PermissionRequest { tool, args, .. } => {
                            let label =
                                permission_activity_label(&ToolCall::summarize_activity(&tool, &args));
                            phase.note_permission_request(&tool, &args);
                            set_plan_step(&mut ui_state, &label);
                            push_activity_row(&mut ui_state, &label);
                            ui_state.permission_bar.set_pending(label.clone());
                            sync_permission_overlay(
                                &mut ui_state,
                                &tool,
                                &args.to_string(),
                                "Awaiting approval",
                                true,
                            );
                            ui_state.notifications.warn(label);
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        EngineEvent::PermissionExplanation { tool, explanation } => {
                            if let Some(detail) = ui_state.permission_detail.as_mut() {
                                if detail.tool.eq_ignore_ascii_case(&tool) {
                                    detail.risk_summary = explanation.clone();
                                }
                            }
                            if matches!(
                                ui_state.overlays.top(),
                                Some(Overlay::PermissionDialog { .. })
                            ) {
                                let args = ui_state
                                    .permission_detail
                                    .as_ref()
                                    .map(|detail| detail.args.clone())
                                    .unwrap_or_default();
                                sync_permission_overlay(
                                    &mut ui_state,
                                    &tool,
                                    &args,
                                    &explanation,
                                    true,
                                );
                            }
                            self.persist_interactive_snapshot(&ui_state);
                        }
                        _ => {}
                    }
                }

                Some(request) = async {
                    confirmation_rx.as_mut().unwrap().recv().await
                }, if confirmation_rx.is_some() => {
                    let args = permission_request_args(&request.arguments);
                    let label =
                        permission_activity_label(&ToolCall::summarize_activity(&request.tool_name, &args));
                    phase.note_permission_request(&request.tool_name, &args);
                    set_plan_step(&mut ui_state, &label);
                    push_activity_row(&mut ui_state, &label);
                    ui_state.permission_bar.set_pending(label.clone());
                    sync_permission_overlay(
                        &mut ui_state,
                        &request.tool_name,
                        &request.arguments,
                        &request.reason,
                        request.allow_session,
                    );
                    ui_state.notifications.warn(&label);
                    pending_confirmation = Some(request);
                    self.persist_interactive_snapshot(&ui_state);
                }

                Some((engine, result)) = async {
                    query_done_rx.as_mut().unwrap().await.ok()
                }, if query_done_rx.is_some() => {
                    self.engine = Some(engine);
                    query_done_rx = None;
                    elicit_rx = None;
                    tui_elicit_rx = None;
                    self.pending_elicitation = None;
                    pending_confirmation = None;
                    thinking = false;
                    phase.reset();
                    complete_plan(&mut ui_state);
                    clear_permission_overlay(&mut ui_state);

                    if let Some(last) = ui_state
                        .messages
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|msg| msg.role == MessageRole::Assistant && msg.streaming)
                    {
                        last.streaming = false;
                    }

                    match result {
                        Ok(TerminalReason::ApiUnavailable { message }) => {
                            ui_state.messages.push(Message {
                                role: MessageRole::System,
                                content: format!(
                                    "The AI service is temporarily unavailable. Your conversation context is preserved. Please try again later.\n({message})"
                                ).into(),
                                tool_name: None,
                                streaming: false,
                            folded: false,
                            });
                        }
                        Ok(_) => {
                            let turn_start = ui_state
                                .messages
                                .messages
                                .iter()
                                .rposition(|msg| msg.role == MessageRole::User)
                                .unwrap_or(0);
                            let turn_slice = &ui_state.messages.messages[turn_start..];
                            let assistant_empty = turn_slice
                                .iter()
                                .rev()
                                .find(|msg| msg.role == MessageRole::Assistant)
                                .is_some_and(|msg| msg.content.trim().is_empty());
                            let had_tool_activity = turn_slice.iter().any(|msg| {
                                matches!(
                                    msg.role,
                                    MessageRole::ToolCall | MessageRole::ToolResult
                                )
                            });
                            if assistant_empty && !had_tool_activity {
                                let message =
                                    "Query failed: provider returned no visible response.".to_string();
                                ui_state.notifications.err(&message);
                                ui_state.messages.push(Message {
                                    role: MessageRole::System,
                                    content: message.into(),
                                    tool_name: None,
                                    streaming: false,
                                    folded: false,
                                });
                            }
                            ui_state.notifications.success("Turn completed");
                            // Trigger background features after successful turn
                            let prompt_text = self.history.last().cloned().unwrap_or_default();
                            self.trigger_background_features(&prompt_text).await;
                        }
                        Err(e) => {
                            ui_state.notifications.err("Turn failed");
                            ui_state.messages.push(Message {
                                role: MessageRole::System,
                                content: format!("Query failed: {e}").into(),
                                tool_name: None,
                                streaming: false,
                            folded: false,
                            });
                            show_detail_overlay(
                                &mut ui_state,
                                "Failure Details",
                                format!("Query failed: {e}"),
                            );
                        }
                    }

                    self.persist_interactive_snapshot(&ui_state);

                    if let Some(next_prompt) = queued_prompts.pop_front() {
                            self.start_tui_query(
                                next_prompt,
                                &mut ui_state,
                                &mut thinking,
                                &mut phase,
                                &mut query_done_rx,
                                &mut elicit_rx,
                                &mut tui_elicit_rx,
                            );
                    }
                }

                Some((job_id, prompt)) = async {
                    self.cron_rx.as_mut().unwrap().recv().await
                }, if self.cron_rx.is_some() => {
                    ui_state.messages.push(Message {
                        role: MessageRole::System,
                        content: format!("[Cron {job_id}] {prompt}").into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                }

                Some(request) = async {
                    elicit_rx.as_mut().unwrap().recv().await
                }, if elicit_rx.is_some() => {
                    for (i, q) in request.questions.iter().enumerate() {
                        let text = q.get("question").and_then(|v| v.as_str()).unwrap_or("?");
                        ui_state.messages.push(Message {
                            role: MessageRole::System,
                            content: format!("[Question {}] {}", i + 1, text).into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        if let Some(options) = q.get("options").and_then(|v| v.as_array()) {
                            for opt in options {
                                if let (Some(label), Some(desc)) = (
                                    opt.get("label").and_then(|v| v.as_str()),
                                    opt.get("description").and_then(|v| v.as_str()),
                                ) {
                                    ui_state.messages.push(Message {
                                        role: MessageRole::System,
                                        content: format!("  {} — {}", label, desc).into(),
                                        tool_name: None,
                                        streaming: false,
                                        folded: false,
                                    });
                                }
                            }
                        }
                    }
                    ui_state.messages.push(Message {
                        role: MessageRole::System,
                        content: "Type your answer and press Enter.".into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                    self.pending_elicitation = Some(request.response_tx);
                }

                Some(request) = async {
                    tui_elicit_rx.as_mut().unwrap().recv().await
                }, if tui_elicit_rx.is_some() => {
                    thinking = false;
                    phase.reset();
                    if let Some(last) = ui_state
                        .messages
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|msg| msg.role == MessageRole::Assistant && msg.streaming)
                    {
                        last.streaming = false;
                    }
                    ui_state.notifications.info(&request.prompt);
                    self.pending_elicitation = Some(request.response_tx);
                }

                else => break Ok(()),
            }
        };

        // Cleanup
        let _ = terminal_ui.exit();
        let _ = disable_raw_mode();
        self.event_rx = event_rx;

        result
    }

    /// Run a single query in non-interactive (headless) mode.
    pub async fn run_headless(&mut self, prompt: &str) -> Result<String> {
        if prompt.starts_with('/') {
            // Check for macro expansion
            let parts: Vec<&str> = prompt.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            if self.macro_registry.has(cmd_name) {
                if let Some(commands) = self.macro_registry.get(cmd_name) {
                    let cmds: Vec<String> = commands.to_vec();
                    let mut outputs = Vec::new();
                    for macro_cmd in cmds {
                        let out = Box::pin(self.run_headless(&macro_cmd)).await?;
                        outputs.push(out);
                    }
                    return Ok(outputs.join("\n"));
                }
            }

            // Check for pipe operator
            if prompt.contains('|') {
                let segments: Vec<&str> = prompt.trim_start_matches('/').split('|').collect();
                let mut pipe_input: Option<String> = None;
                let mut outputs = Vec::new();
                for segment in &segments {
                    let segment = segment.trim();
                    if segment.is_empty() {
                        continue;
                    }
                    let p: Vec<&str> = segment.split_whitespace().collect();
                    let name = p[0];
                    let args: Vec<String> = p[1..].iter().map(|s| s.to_string()).collect();
                    self.command_history.append(name, &args);
                    let mut ctx = self.build_context();
                    ctx.pipe_input = pipe_input;
                    let result = self.commands.execute(name, &ctx, &args);
                    self.sync_command_state(name, &result);
                    outputs.push(result.output.clone());
                    pipe_input = Some(result.output);
                }
                return Ok(outputs.join("\n---\n"));
            }

            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
            self.command_history.append(cmd_name, &args);
            let ctx = self.build_context();
            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);
            Ok(result.output)
        } else if self.engine.is_some() {
            // 14.1: Route to QueryEngine instead of placeholder
            Ok(self.submit_prompt(prompt, true).await)
        } else {
            return Err(anyhow::anyhow!(
                "No QueryEngine configured. Set ANTHROPIC_API_KEY or run `coderex --api-key <key>`"
            ));
        }
    }

    /// Drain pending events from the event channel, append text deltas to output,
    /// and forward all events into the SDK session.
    fn drain_events(&mut self, output: &mut String) {
        let sdk = self.sdk_session.clone();
        if let Some(ref mut rx) = self.event_rx {
            while let Ok(event) = rx.try_recv() {
                if let Some(ref session) = sdk {
                    session.emit(SdkEvent::Engine(event.clone()));
                }
                if let EngineEvent::TextDelta(delta) = event {
                    output.push_str(&delta);
                }
            }
        }
    }

    fn normalize_headless_read_output(&self, prompt: &str, mut output: String) -> String {
        let Some(path) = Self::extract_read_path(prompt) else {
            return output;
        };
        let resolved = self.resolve_read_path_for_fallback(&path);
        let Ok(content) = std::fs::read_to_string(&resolved) else {
            return output;
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return output;
        }

        while let Some(first) = output.find(trimmed) {
            let search_start = first + trimmed.len();
            let Some(second_rel) = output[search_start..].find(trimmed) else {
                break;
            };
            let second = search_start + second_rel;
            output.replace_range(second..second + trimmed.len(), "");
        }

        if let Some(first) = output.find(trimmed) {
            let prefix = &output[..first];
            let lowered = prefix.to_ascii_lowercase();
            let looks_like_planning_prefix = lowered.contains("i'll read")
                || lowered.contains("let me")
                || lowered.contains("<function_calls>")
                || lowered.contains("<antthinking>")
                || lowered.contains("<invoke name=")
                || lowered.contains("tool:")
                || lowered.contains("path:")
                || lowered.contains("read the file");
            if looks_like_planning_prefix {
                output = output[first..].to_string();
            }
        }

        output
    }

    /// Build a QueryRequest for the main agent from current session effects.
    fn build_query_request_for_main_agent(&self) -> QueryRequest {
        QueryRequest {
            kind: QueryKind::MainAgent,
            user_prompt: String::new(), // filled at submit time
            // If override is set, it replaces the base prompt; otherwise None
            custom_system_prompt: self.session_effects.override_system_prompt.clone(),
            append_system_prompt: self.session_effects.append_system_prompt.clone(),
            prompt_profile: self
                .session_effects
                .prompt_profiles
                .last()
                .copied()
                .unwrap_or_default(),
            allowed_tools: self.session_effects.allowed_tools_override.clone(),
            model_profile: ModelProfile::Main,
            inherit_session: true,
            stream: true,
            max_tokens: 4096,
            agent_id: None,
            provider_hints: HashMap::new(),
        }
    }

    /// Build SubmitOptions incorporating active command effects.
    ///
    /// Routes through the Orchestrator so that prompt profiles, model selection,
    /// and tool whitelists are applied consistently with side/forked queries.
    fn build_submit_options(&mut self) -> SubmitOptions {
        let request = self.build_query_request_for_main_agent();
        let mut options = if let Some(ref orchestrator) = self.orchestrator {
            orchestrator.build_submit_options(&request)
        } else {
            // Fallback when no orchestrator is available
            let mut opts = SubmitOptions::default();
            if let Some(ref override_prompt) = self.session_effects.override_system_prompt {
                // Override takes precedence over profile-based composition
                opts.custom_system_prompt = Some(override_prompt.clone());
            } else if !self.session_effects.prompt_profiles.is_empty() {
                opts.custom_system_prompt = Some(prompts::compose_system_prompt_for_profiles(
                    &self.session_effects.prompt_profiles,
                ));
            }
            if let Some(ref prompt) = self.session_effects.append_system_prompt {
                opts.append_system_prompt = Some(prompt.clone());
            }
            if let Some(ref tools) = self.session_effects.allowed_tools_override {
                opts.allowed_tools_override = Some(tools.clone());
            } else if let Some(default_tools) =
                prompts::default_allowed_tools_for_profiles(&self.session_effects.prompt_profiles)
            {
                opts.allowed_tools_override = Some(default_tools);
            }
            opts
        };

        // REPL-specific: extra context from commands is not part of QueryRequest
        if !self.session_effects.extra_context.is_empty() {
            options.replay_user_messages = Some(
                self.session_effects
                    .extra_context
                    .iter()
                    .cloned()
                    .map(|ctx| Message::user(format!("Additional context: {ctx}")))
                    .collect(),
            );
        }

        // Apply CLI flag overrides
        if let Some(ref schema) = self.json_schema {
            let schema_val: Result<serde_json::Value, _> =
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(schema) {
                    Ok(val)
                } else if let Ok(contents) = std::fs::read_to_string(schema) {
                    serde_json::from_str(&contents)
                } else {
                    Err(serde_json::from_str::<serde_json::Value>("").unwrap_err())
                };
            if let Ok(val) = schema_val {
                options.json_schema = Some(val.clone());
                // Configure the engine's structured output validator
                if let Some(ref mut engine) = self.engine {
                    engine.set_structured_output(val);
                }
            }
        }
        if let Some(budget_usd) = self.max_budget_usd {
            if budget_usd > 0.0 {
                // Convert USD to tokens using a conservative rate ($3 per 1M tokens)
                let tokens = ((budget_usd / 3.0) * 1_000_000.0).max(1.0) as u64;
                options.task_budget_tokens = tokens;
            }
        }
        if let Some(turns) = self.max_turns {
            options.max_turns = turns;
        }
        options.include_partial_messages = self.include_partial_messages;
        // In the interactive REPL, assistant clarification questions should stay in the
        // transcript as normal messages. The engine's fallback elicitation prompt writes to
        // stderr/stdin directly, which breaks the TUI and duplicates the normal next-turn flow.
        options.handle_elicitation = false;

        options
    }

    /// Trigger background features after a successful turn.
    ///
    /// Tasks are tracked in `self.background_tasks` with cancellation tokens.
    /// Results are delivered asynchronously via `self.background_results_rx`.
    async fn trigger_background_features(&mut self, _user_prompt: &str) {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return,
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return,
        };

        self.cleanup_finished_background_tasks();

        // Trigger prompt suggestions after the user has sent a few messages
        if self.history.len() >= 3 && self.history.len() % 5 == 0 {
            let conversation_summary = self
                .history
                .iter()
                .rev()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let request = BackgroundFeatureRequest {
                kind: BackgroundFeatureKind::PromptSuggestion,
                system_prompt: "You are a prompt suggestion assistant.".into(),
                user_prompt: format!(
                    "Suggest 1-2 brief follow-up prompts based on this conversation context.\n\
                     Recent messages:\n{conversation_summary}\n\
                     Return only the suggestions, one per line."
                ),
                parent_context: None,
                allowed_tools: None,
                max_tokens: 128,
                writable: false,
            };
            self.spawn_background_task(
                "prompt_suggestion",
                BackgroundFeatureKind::PromptSuggestion,
                client,
                request,
                tx,
            );
        }
    }

    /// Spawn a tracked background task with cancellation support.
    fn spawn_background_task(
        &mut self,
        id: &str,
        kind: BackgroundFeatureKind,
        client: Box<dyn ApiClient>,
        request: BackgroundFeatureRequest,
        result_tx: mpsc::UnboundedSender<BackgroundTaskResult>,
    ) {
        let cancel_token = CancellationToken::new();
        let task_id = id.to_string();
        let task_cancel = cancel_token.clone();

        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind,
            cancel_token: cancel_token.clone(),
        });

        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = coderex_core::orchestrator::run_background_feature(
                    client.as_ref(), request
                ) => res,
            };

            let (success, content) = match result {
                Ok(r) => (true, r.content),
                Err(e) => (false, e.to_string()),
            };

            let _ = result_tx.send(BackgroundTaskResult {
                id: task_id,
                kind,
                success,
                content,
            });
        });
    }

    /// Cancel all running background tasks.
    fn cancel_background_tasks(&mut self) {
        for task in &self.background_tasks {
            task.cancel_token.cancel();
            tracing::info!("Cancelled background task: {} ({:?})", task.id, task.kind);
        }
        self.background_tasks.clear();
    }

    /// Remove finished (cancelled or completed) background tasks from tracking.
    fn cleanup_finished_background_tasks(&mut self) {
        self.background_tasks
            .retain(|t| !t.cancel_token.is_cancelled());
    }

    /// Poll for completed background task results and return them.
    fn poll_background_results(&mut self) -> Vec<BackgroundTaskResult> {
        let mut results = Vec::new();
        if let Some(ref mut rx) = self.background_results_rx {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }
        results
    }

    /// Run agent generation synchronously and return the result.
    async fn run_agent_generation(&mut self, description: &str) -> String {
        let Some(ref client) = self.api_client else {
            return "No API client configured.".into();
        };
        match coderex_core::prompts::generate_agent(client.as_ref(), description).await {
            Ok(spec) => format!(
                "Agent Generated\n──────────────\nName: {}\nDescription: {}\nModel: {}\nAllowed Tools: {}\n\nSystem Prompt:\n{}",
                spec.name,
                spec.description,
                spec.model_profile,
                spec.allowed_tools
                    .as_ref()
                    .map(|t| t.join(", "))
                    .unwrap_or_else(|| "all".into()),
                spec.system_prompt
            ),
            Err(e) => format!("Agent generation failed: {e}"),
        }
    }

    /// Compact the session context by truncating older messages.
    /// Keeps the first user message and the most recent 3 turns.
    #[allow(dead_code)]
    async fn compact_context(&mut self, ui_state: &mut coderex_ui::state::UiState) -> String {
        let Some(engine) = self.engine.as_mut() else {
            return "No engine configured.".into();
        };
        let result = engine.compact_session_messages(3);
        if result.tokens_removed == 0 {
            return "Context is already compact.".into();
        }
        // Sync UI message list: rebuild from compacted messages
        restore_transcript_messages(ui_state, &result.messages);
        self.persist_interactive_snapshot(ui_state);
        format!(
            "Context compacted: {} tokens removed, {} messages remaining.",
            result.tokens_removed,
            result.messages.len()
        )
    }

    /// Compact context in non-TUI mode.
    async fn compact_context_non_tui(&mut self) -> String {
        let Some(ref mut engine) = self.engine else {
            return "No engine configured.".into();
        };
        let result = engine.compact_session_messages(3);
        if result.tokens_removed == 0 {
            return "Context is already compact.".into();
        }
        format!(
            "Context compacted: {} tokens removed, {} messages remaining.",
            result.tokens_removed,
            result.messages.len()
        )
    }

    /// Spawn a background task that updates session memory via ai_workflow.
    async fn spawn_memory_update_task(&mut self) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };
        let store = match &self.session_memory_store {
            Some(s) => Arc::clone(s),
            None => return "Session memory store not available.".into(),
        };
        let session_id = match self.session_id {
            Some(s) => s,
            None => return "Session ID not available.".into(),
        };
        let conversation = self
            .history
            .iter()
            .rev()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let current_memory = store
            .get_facts(session_id)
            .into_iter()
            .map(|f| f.fact)
            .collect::<Vec<_>>()
            .join("\n");

        self.cleanup_finished_background_tasks();
        let task_id = format!("memory_update_{}", self.background_tasks.len());
        let cancel_token = CancellationToken::new();
        let task_cancel = cancel_token.clone();
        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind: BackgroundFeatureKind::SessionMemoryUpdate,
            cancel_token: cancel_token.clone(),
        });

        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = ai_workflow::update_session_memory_ai(
                    client.as_ref(), &store, session_id, &current_memory, &conversation
                ) => res.map_err(|e| coderex_core::orchestrator::OrchestratorError::Config(e.to_string())),
            };
            let (success, content) = match result {
                Ok(text) => (true, text),
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(BackgroundTaskResult {
                id: task_id_clone,
                kind: BackgroundFeatureKind::SessionMemoryUpdate,
                success,
                content,
            });
        });
        format!("Background task started: {task_id}")
    }

    /// Spawn a background task that extracts long-term memories via ai_workflow.
    async fn spawn_memory_extraction_task(&mut self) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };
        let store = match &self.long_term_store {
            Some(s) => Arc::clone(s),
            None => return "Long-term memory store not available.".into(),
        };
        let conversation = self
            .history
            .iter()
            .rev()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");

        self.cleanup_finished_background_tasks();
        let task_id = format!("memory_extraction_{}", self.background_tasks.len());
        let cancel_token = CancellationToken::new();
        let task_cancel = cancel_token.clone();
        self.background_tasks.push(BackgroundTask {
            id: task_id.clone(),
            kind: BackgroundFeatureKind::MemoryExtraction,
            cancel_token: cancel_token.clone(),
        });

        let task_id_clone = task_id.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = task_cancel.cancelled() => {
                    Err(coderex_core::orchestrator::OrchestratorError::Config(
                        "Cancelled by user".into()
                    ))
                }
                res = ai_workflow::extract_long_term_memories_ai(
                    client.as_ref(), &store, &conversation
                ) => res.map_err(|e| coderex_core::orchestrator::OrchestratorError::Config(e.to_string())),
            };
            let (success, content) = match result {
                Ok(topics) => (true, topics.join(", ")),
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(BackgroundTaskResult {
                id: task_id_clone,
                kind: BackgroundFeatureKind::MemoryExtraction,
                success,
                content,
            });
        });
        format!("Background task started: {task_id}")
    }

    /// Handle a completed session-memory-update result (no-op: already persisted in task).
    async fn handle_memory_update_result(&mut self, _result: &BackgroundTaskResult) {}

    /// Handle a completed memory-extraction result (no-op: already persisted in task).
    async fn handle_memory_extraction_result(&mut self, _result: &BackgroundTaskResult) {}

    /// Spawn a background feature by kind with a sensible default request.
    fn spawn_background_feature_by_kind(
        &mut self,
        kind: BackgroundFeatureKind,
        user_prompt: String,
    ) -> String {
        let client = match &self.api_client {
            Some(c) => c.clone_box(),
            None => return "No API client configured.".into(),
        };
        let tx = match &self.background_results_tx {
            Some(t) => t.clone(),
            None => return "Background task channel not available.".into(),
        };

        self.cleanup_finished_background_tasks();

        let (system_prompt, max_tokens) = match kind {
            BackgroundFeatureKind::AgentSummary => (
                "You are a sub-agent summarizer. Produce a concise summary of recent activity."
                    .into(),
                256,
            ),
            BackgroundFeatureKind::AwaySummary => (
                "You are a session summarizer. Summarize what happened while the user was away."
                    .into(),
                256,
            ),
            BackgroundFeatureKind::MagicDocs => (
                "You are a documentation generator. Produce concise code documentation.".into(),
                1024,
            ),
            BackgroundFeatureKind::CodebaseOverview => (
                "You are a codebase analyst. Produce a high-level overview of the project.".into(),
                1024,
            ),
            BackgroundFeatureKind::ReadmeGeneration => (
                "You are a README generator. Produce a project README in Markdown.".into(),
                2048,
            ),
            BackgroundFeatureKind::SessionMemoryUpdate => (
                "You are a memory manager. Update session memory based on recent conversation."
                    .into(),
                512,
            ),
            BackgroundFeatureKind::MemoryExtraction => (
                "You are a memory extractor. Identify long-term memories from this session.".into(),
                512,
            ),
            BackgroundFeatureKind::PromptSuggestion => {
                ("You are a prompt suggestion assistant.".into(), 128)
            }
            BackgroundFeatureKind::NextActionSuggestion => (
                "You are a task planner. Suggest the next best action.".into(),
                128,
            ),
            BackgroundFeatureKind::TeamStatus => ("You are a team status aggregator.".into(), 256),
        };

        let parent_context = if self.history.len() >= 3 {
            Some(
                self.history
                    .iter()
                    .rev()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        } else {
            None
        };

        let request = BackgroundFeatureRequest {
            kind,
            system_prompt,
            user_prompt,
            parent_context,
            allowed_tools: None,
            max_tokens,
            writable: matches!(
                kind,
                BackgroundFeatureKind::SessionMemoryUpdate
                    | BackgroundFeatureKind::MemoryExtraction
            ),
        };

        let id = format!(
            "{}_{}",
            background_feature_label(kind),
            self.background_tasks.len()
        );
        self.spawn_background_task(&id, kind, client, request, tx);
        format!("Background task started: {id}")
    }

    /// Apply effects from a CommandResult to session state.
    fn apply_command_effects(&mut self, result: &CommandResult) {
        if let Some(profile) = result.prompt_profile {
            if !self.session_effects.prompt_profiles.contains(&profile) {
                self.session_effects.prompt_profiles.push(profile);
            }
        }

        if let Some(ref prompt) = result.override_system_prompt {
            // Override takes precedence: replace the base prompt entirely
            self.session_effects.override_system_prompt = Some(prompt.clone());
        } else if let Some(ref prompt) = result.append_system_prompt {
            if let Some(ref existing) = self.session_effects.append_system_prompt {
                self.session_effects.append_system_prompt = Some(format!("{existing}\n{prompt}"));
            } else {
                self.session_effects.append_system_prompt = Some(prompt.clone());
            }
        }

        if let Some(ref ctx) = result.extra_context {
            self.session_effects.extra_context.push(ctx.clone());
        }

        if let Some(ref tools) = result.allowed_tools_override {
            self.session_effects.allowed_tools_override = Some(tools.clone());
        }
    }

    /// Apply command effects that mutate persistent REPL state.
    fn sync_command_state(&mut self, cmd_name: &str, result: &CommandResult) -> bool {
        self.apply_command_effects(result);

        if cmd_name == "vim" && result.state_changed {
            self.vim_mode_enabled = !self.vim_mode_enabled;
            return true;
        }

        if cmd_name == "theme" && result.state_changed {
            if let Some(theme_name) = result.output.strip_prefix("Theme changed to: ") {
                self.current_theme = theme_name.trim().to_string();
                return true;
            }
        }

        false
    }

    fn apply_tui_theme(&self, ui_state: &mut coderex_ui::state::UiState, theme_name: &str) -> bool {
        let manager = coderex_ui::themes::ThemeManager::new();
        if let Some(theme) = manager.get_theme(theme_name) {
            ui_state.theme = theme.clone();
            true
        } else {
            false
        }
    }

    fn extract_read_path(prompt: &str) -> Option<String> {
        let mut parts = prompt.split_whitespace();
        let first = parts.next()?;
        if !first.eq_ignore_ascii_case("read") {
            return None;
        }
        let raw = parts.next()?;
        let cleaned = raw
            .trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | ',' | ';' | ':' | ')' | '('))
            .to_string();
        if cleaned.is_empty() || (!cleaned.contains('/') && !cleaned.contains('.')) {
            return None;
        }
        Some(cleaned)
    }

    fn resolve_read_path_for_fallback(&self, file_path: &str) -> std::path::PathBuf {
        let path = std::path::PathBuf::from(file_path);
        if path.is_absolute() {
            if path.exists() {
                return path;
            }
            if let Ok(trimmed) = path.strip_prefix("/") {
                let candidate = std::path::PathBuf::from(&self.cwd).join(trimmed);
                if candidate.exists() {
                    return candidate;
                }
            }
            path
        } else {
            std::path::PathBuf::from(&self.cwd).join(path)
        }
    }

    async fn submit_prompt(&mut self, prompt: &str, headless_mode: bool) -> String {
        let (elicit_tx, mut elicit_rx) =
            tokio::sync::mpsc::channel::<coderex_tools::ElicitRequest>(4);

        if let Some(ref dispatcher) = self.dispatcher {
            dispatcher.set_elicit_tx(Some(elicit_tx));
        }

        let mut output = String::new();
        self.drain_events(&mut output);

        let options = self.build_submit_options();
        let mut engine = self.engine.take().unwrap();
        let prompt_owned = prompt.to_string();
        let (engine_tx, engine_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = engine.submit_streaming(prompt_owned, options).await;
            let _ = engine_tx.send((engine, result));
        });

        let mut engine_rx = Some(engine_rx);
        let (engine, result) = loop {
            tokio::select! {
                Some(request) = elicit_rx.recv() => {
                    for (i, q) in request.questions.iter().enumerate() {
                        if let Some(text) = q.get("question").and_then(|v| v.as_str()) {
                            println!("  [{}] {}", i + 1, text);
                        }
                        if let Some(options) = q.get("options").and_then(|v| v.as_array()) {
                            for opt in options {
                                if let (Some(label), Some(desc)) = (
                                    opt.get("label").and_then(|v| v.as_str()),
                                    opt.get("description").and_then(|v| v.as_str()),
                                ) {
                                    println!("     {} — {}", label, desc);
                                }
                            }
                        }
                    }
                    print!("Your answer: ");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                    let answer = line.trim();
                    let answers = serde_json::json!({ "answer": answer });
                    let _ = request.response_tx.send(answers);
                }
                Ok((engine, result)) = async { engine_rx.take().unwrap().await }, if engine_rx.is_some() => {
                    break (engine, result);
                }
            }
        };

        self.engine = Some(engine);

        self.drain_events(&mut output);

        // Trigger background features on successful turn
        if result.is_ok() {
            self.trigger_background_features(prompt).await;
        }

        if !self.session_effects.title_generated {
            if let Some(ref client) = self.api_client {
                if let Ok(title) = prompts::generate_session_title(client.as_ref(), prompt).await {
                    self.session_effects.title_generated = true;
                    if let Some(ref mut engine) = self.engine {
                        let mut session = engine.session().clone();
                        if session.title.is_none() {
                            session.title = Some(title.clone());
                            engine.set_session(session);
                        }
                    }
                    if !output.is_empty() {
                        output.insert_str(0, &format!("[Session title: {title}]\n"));
                    } else {
                        output = format!("[Session title: {title}]");
                    }
                }
            }
        }

        match result {
            Ok(TerminalReason::ApiUnavailable { message }) => {
                format!(
                    "The AI service is temporarily unavailable. Your conversation context is preserved. Please try again later.\n({message})"
                )
            }
            Ok(reason) => {
                let mut final_output = if output.is_empty() {
                    format!("Query completed ({reason:?})")
                } else if !matches!(reason, TerminalReason::Completed) {
                    format!("{output}\nQuery completed ({reason:?})")
                } else {
                    output
                };

                if headless_mode {
                    let lower = final_output.to_lowercase();
                    if !lower.contains("pub fn") && !lower.contains("fn ") {
                        if let Some(path) = Self::extract_read_path(prompt) {
                            let resolved = self.resolve_read_path_for_fallback(&path);
                            if let Ok(content) = std::fs::read_to_string(&resolved) {
                                final_output.push_str("\n\n");
                                final_output.push_str(&content);
                            }
                        }
                    }
                    final_output = self.normalize_headless_read_output(prompt, final_output);
                }
                final_output
            }
            Err(e) => format!("Query failed: {e}"),
        }
    }

    /// Sync REPL state into the TUI statusline.
    fn sync_statusline(&self, statusline: &mut coderex_ui::statusline::Statusline) {
        statusline.model = self.model.0.clone();
        statusline.session_title = if self.session_effects.title_generated {
            "Active".into()
        } else {
            "New Session".into()
        };
        statusline.git_branch = current_git_branch(&self.cwd);
        statusline.total_tokens = self.history.len() * 100;
        statusline.cost_usd = 0.0;
    }

    /// Sync current permission mode into the TUI permission bar.
    fn sync_permission_bar(&self, bar: &mut coderex_ui::permission_bar::PermissionBar) {
        if let Some(ref engine) = self.engine {
            let checker = engine.permission_checker();
            if let Ok(c) = checker.lock() {
                bar.mode = c.mode();
            }
        }
    }

    /// Update slash-command suggestion overlay based on current input.
    fn update_slash_suggestions(&self, ui_state: &mut coderex_ui::state::UiState) {
        use coderex_ui::overlay::Overlay;

        if ui_state.input.text.starts_with('/') {
            let query = ui_state.input.text.trim_start_matches('/');
            let cmds: Vec<String> = self
                .commands
                .list()
                .iter()
                .filter(|c| query.is_empty() || c.name.contains(query))
                .map(|c| format!("/{} - {}", c.name, c.description))
                .collect();
            // Replace existing palette overlay so suggestions stay in sync.
            if let Some(top) = ui_state.overlays.top() {
                if matches!(top, Overlay::CommandPalette { .. }) {
                    ui_state.overlays.pop();
                }
            }
            ui_state.overlays.push(Overlay::CommandPalette {
                query: ui_state.input.text.clone(),
                commands: cmds,
                selected: 0,
            });
        } else {
            // Remove palette overlay when input no longer starts with '/'.
            if let Some(top) = ui_state.overlays.top() {
                if matches!(top, Overlay::CommandPalette { .. }) {
                    ui_state.overlays.pop();
                }
            }
        }
    }

    /// Process a line of input in the TUI context.
    async fn process_tui_input(
        &mut self,
        text: &str,
        ui_state: &mut coderex_ui::state::UiState,
        thinking: &mut bool,
        phase: &mut InteractivePhase,
        query_done_rx: &mut Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        >,
        elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<coderex_tools::ElicitRequest>>,
        tui_elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<TuiElicitationRequest>>,
        queued_prompts: &mut std::collections::VecDeque<String>,
    ) {
        // Clear previous turn's duration display so it doesn't linger.
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking_started = None;
        ui_state.statusline.spinner_frame = 0;
        ui_state.messages.scroll_to_bottom();

        // If there's a pending elicitation, treat this input as the answer.
        if let Some(tx) = self.pending_elicitation.take() {
            self.history.push(text.to_string());
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::User,
                content: text.into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
            let answer = serde_json::json!({ "answer": text });
            let _ = tx.send(answer);
            *thinking = true;
            *phase = InteractivePhase::Thinking;
            self.persist_interactive_snapshot(ui_state);
            return;
        }

        self.history.push(text.to_string());

        // Add user message
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::User,
            content: text.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });

        if text.starts_with('/') {
            let parts: Vec<&str> = text.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

            // Built-in REPL commands that bypass the registry
            match cmd_name {
                "exit" | "quit" => {
                    self.running = false;
                    return;
                }
                "tokens" => {
                    self.execute_and_display_command("usage", &[], ui_state)
                        .await;
                    return;
                }
                "compact" => {
                    let msg = self.compact_context(ui_state).await;
                    ui_state.notifications.info(&msg);
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: msg.into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                    return;
                }
                "model" => {
                    if let Some(model) = args.first() {
                        let mapped = match model.to_lowercase().as_str() {
                            "opus" => ModelId::opus(),
                            "sonnet" => ModelId::sonnet(),
                            "haiku" => ModelId::haiku(),
                            other => ModelId(other.to_string()),
                        };
                        self.model = mapped;
                    }
                    self.execute_and_display_command(cmd_name, &args, ui_state)
                        .await;
                    return;
                }
                "vim" => {
                    let ctx = self.build_context();
                    let result = self.commands.execute("vim", &ctx, &args);
                    let toggled = self.sync_command_state("vim", &result);
                    if toggled {
                        ui_state.notifications.info(&result.output);
                    }
                    ui_state.messages.push(coderex_ui::message_list::Message {
                        role: coderex_ui::message_list::MessageRole::System,
                        content: result.output.into(),
                        tool_name: None,
                        streaming: false,
                        folded: false,
                    });
                    return;
                }
                "plan" => {
                    let registry_output =
                        self.commands.execute("plan", &self.build_context(), &args);
                    let body = ui_state.plan.render_detail();
                    show_detail_overlay(ui_state, "Current Plan", body);
                    if !registry_output.output.is_empty() {
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: registry_output.output.into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                    }
                    return;
                }
                "resume" => {
                    if *thinking || query_done_rx.is_some() {
                        let message =
                            "Finish or cancel the active turn before resuming another session.";
                        ui_state.notifications.warn(message);
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: message.into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        return;
                    }
                    match self.resume_tui_session(args.first().map(String::as_str), ui_state) {
                        Ok(_) => {}
                        Err(err) => {
                            ui_state.notifications.err(&err);
                            ui_state.messages.push(coderex_ui::message_list::Message {
                                role: coderex_ui::message_list::MessageRole::System,
                                content: format!("Error: {err}").into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                        }
                    }
                    return;
                }
                "activity" => {
                    let body = if ui_state.activity_log.is_empty() {
                        "No recent activity yet.".to_string()
                    } else {
                        ui_state.activity_log.join("\n")
                    };
                    show_detail_overlay(ui_state, "Recent Activity", body);
                    return;
                }
                "open" => {
                    if args.is_empty() {
                        ui_state.messages.push(coderex_ui::message_list::Message {
                            role: coderex_ui::message_list::MessageRole::System,
                            content: "Usage: /open <path>".into(),
                            tool_name: None,
                            streaming: false,
                            folded: false,
                        });
                        ui_state.notifications.warn("Usage: /open <path>");
                        return;
                    }
                    let raw_path = std::path::PathBuf::from(&args[0]);
                    let path = if raw_path.is_absolute() {
                        raw_path
                    } else {
                        std::path::Path::new(&self.cwd).join(raw_path)
                    };
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            ui_state.left_pane_content = content;
                            ui_state.left_pane_title = args[0].clone();
                            ui_state.layout_mode =
                                coderex_ui::state::LayoutMode::HorizontalSplit { left_percent: 50 };
                            ui_state
                                .notifications
                                .success(format!("Opened {}", args[0]));
                            push_activity_row(ui_state, &format!("Opened {}", args[0]));
                            self.persist_interactive_snapshot(ui_state);
                        }
                        Err(err) => {
                            let detail = format!("Failed to open {}: {}", args[0], err);
                            ui_state.notifications.err(&detail);
                            show_detail_overlay(ui_state, "Failure Details", detail.clone());
                            ui_state.messages.push(coderex_ui::message_list::Message {
                                role: coderex_ui::message_list::MessageRole::System,
                                content: detail.into(),
                                tool_name: None,
                                streaming: false,
                                folded: false,
                            });
                        }
                    }
                    return;
                }
                _ => {}
            }

            // Check for macro expansion first
            if self.macro_registry.has(cmd_name) {
                if let Some(commands) = self.macro_registry.get(cmd_name) {
                    let cmds: Vec<String> = commands.to_vec();
                    for macro_cmd in cmds {
                        self.execute_single_command(&macro_cmd, ui_state).await;
                    }
                    return;
                }
            }

            // Check for pipe operator
            if text.contains('|') {
                self.execute_piped_commands(text, ui_state).await;
                return;
            }

            // Check for destructive command confirmation
            if let Some(cmd) = self.commands.find(cmd_name) {
                if cmd.destructive {
                    self.pending_destructive_cmd = Some((cmd_name.to_string(), args));
                    ui_state
                        .overlays
                        .push(coderex_ui::overlay::Overlay::Confirm {
                            message: format!(
                                "Are you sure you want to run /{}? This action may be destructive.",
                                cmd.name
                            ),
                        });
                    return;
                }
            }

            // Execute the command directly
            self.execute_and_display_command(cmd_name, &args, ui_state)
                .await;
            return;
        } else if self.engine.is_some() {
            self.start_tui_query(
                text.to_string(),
                ui_state,
                thinking,
                phase,
                query_done_rx,
                elicit_rx,
                tui_elicit_rx,
            );
        } else if *thinking || query_done_rx.is_some() {
            queued_prompts.push_back(text.to_string());
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::System,
                content: "[Queued until current turn finishes]".into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        } else {
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::Assistant,
                content: "[No engine configured]".into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        }
    }

    fn start_tui_query(
        &mut self,
        prompt: String,
        ui_state: &mut coderex_ui::state::UiState,
        thinking: &mut bool,
        phase: &mut InteractivePhase,
        query_done_rx: &mut Option<
            tokio::sync::oneshot::Receiver<(QueryEngine, Result<TerminalReason, String>)>,
        >,
        elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<coderex_tools::ElicitRequest>>,
        tui_elicit_rx: &mut Option<tokio::sync::mpsc::Receiver<TuiElicitationRequest>>,
    ) {
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking_started = None;
        ui_state.statusline.spinner_frame = 0;
        ui_state.messages.scroll_to_bottom();
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::Assistant,
            content: "".into(),
            tool_name: None,
            streaming: true,
            folded: false,
        });

        let options = self.build_submit_options();
        let mut engine = self.engine.take().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (elicit_tx, erx) = tokio::sync::mpsc::channel(4);
        let (tui_elicit_tx, terx) = tokio::sync::mpsc::channel(1);
        engine.set_elicitation_handler(Box::new(TuiElicitationHandler::new(tui_elicit_tx)));
        if let Some(ref dispatcher) = self.dispatcher {
            dispatcher.set_elicit_tx(Some(elicit_tx));
        }
        self.query_start_time = Some(std::time::Instant::now());
        ui_state.plan.reset();
        ui_state.permission_bar.clear_status();
        set_plan_step(ui_state, "Planning next step");
        push_activity_row(ui_state, "Planning next step");
        self.persist_interactive_snapshot(ui_state);
        tokio::spawn(async move {
            let result = engine.submit_streaming(prompt, options).await;
            let _ = tx.send((engine, result));
        });
        *query_done_rx = Some(rx);
        *elicit_rx = Some(erx);
        *tui_elicit_rx = Some(terx);
        *thinking = true;
        *phase = InteractivePhase::Planning;
    }

    async fn respond_to_confirmation(
        &self,
        request: ConfirmationRequest,
        response: ConfirmationResponse,
        ui_state: &mut coderex_ui::state::UiState,
        phase: &mut InteractivePhase,
    ) {
        if let Some(bus) = self.confirmation_bus.as_ref() {
            let _ = bus.respond(&request.id, response).await;
        }

        let args = permission_request_args(&request.arguments);
        let activity = ToolCall::summarize_activity(&request.tool_name, &args);
        let approved = matches!(
            response,
            ConfirmationResponse::Approved | ConfirmationResponse::ApproveSession
        );
        let resolution = match response {
            ConfirmationResponse::Approved => format!("approved {activity}"),
            ConfirmationResponse::ApproveSession => {
                format!("approved {activity} for the session")
            }
            ConfirmationResponse::Denied => format!("denied {activity}"),
        };

        clear_permission_overlay(ui_state);
        ui_state
            .permission_bar
            .set_resolution(approved, activity.clone());
        push_activity_row(ui_state, &resolution);
        phase.note_tool_end();

        if approved {
            ui_state.notifications.success(&resolution);
        } else {
            ui_state.notifications.warn(&resolution);
        }
        self.persist_interactive_snapshot(ui_state);
    }

    /// Execute a single command through the registry and display the result in the TUI.
    async fn execute_and_display_command(
        &mut self,
        cmd_name: &str,
        args: &[String],
        ui_state: &mut coderex_ui::state::UiState,
    ) {
        self.command_history.append(cmd_name, args);
        let ctx = self.build_context();
        let result = self.commands.execute(cmd_name, &ctx, args);
        self.sync_command_state(cmd_name, &result);
        if cmd_name == "theme" && result.state_changed {
            if let Some(theme_name) = args.first() {
                if self.apply_tui_theme(ui_state, theme_name) {
                    ui_state
                        .notifications
                        .info(format!("Theme changed to: {theme_name}"));
                }
            }
        }

        if cmd_name == "clear" && result.state_changed {
            self.clear_tui_session(ui_state, result.output.clone());
            return;
        }

        ui_state.messages.push(coderex_ui::message_list::Message {
            role: if result.scrollable {
                coderex_ui::message_list::MessageRole::Assistant
            } else {
                coderex_ui::message_list::MessageRole::System
            },
            content: result.output.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
    }

    fn clear_tui_session(&mut self, ui_state: &mut coderex_ui::state::UiState, message: String) {
        let prior_session_id = self.current_session_id();
        self.history.clear();
        ui_state.messages = coderex_ui::message_list::MessageView::new(1000);
        ui_state.overlays.clear();
        ui_state.notifications.dismiss_all();
        ui_state.input = coderex_ui::input::InputBuffer::new();
        ui_state.plan.reset();
        ui_state.activity_log.clear();
        ui_state.permission_bar.clear_status();
        clear_permission_overlay(ui_state);
        ui_state.statusline.last_duration = None;
        ui_state.statusline.thinking = false;
        ui_state.statusline.thinking_label = None;
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::System,
            content: message.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
        self.clear_interactive_snapshot(prior_session_id.as_deref());
    }

    /// Execute piped commands: split on `|`, run sequentially, pass output as pipe_input.
    async fn execute_piped_commands(
        &mut self,
        text: &str,
        ui_state: &mut coderex_ui::state::UiState,
    ) {
        let segments: Vec<&str> = text.trim_start_matches('/').split('|').collect();
        let mut pipe_input: Option<String> = None;
        let mut outputs = Vec::new();

        for segment in &segments {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            let parts: Vec<&str> = segment.split_whitespace().collect();
            let cmd_name = parts[0];
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

            self.command_history.append(cmd_name, &args);

            let mut ctx = self.build_context();
            ctx.pipe_input = pipe_input;

            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);
            outputs.push(result.output.clone());
            pipe_input = Some(result.output);
        }

        let combined = outputs.join("\n---\n");
        ui_state.messages.push(coderex_ui::message_list::Message {
            role: coderex_ui::message_list::MessageRole::Assistant,
            content: combined.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
    }

    /// Execute a single command string (from macro expansion) in the TUI.
    async fn execute_single_command(
        &mut self,
        cmd_str: &str,
        ui_state: &mut coderex_ui::state::UiState,
    ) {
        let trimmed = cmd_str.trim();
        if trimmed.starts_with('/') {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
            self.command_history.append(cmd_name, &args);
            let ctx = self.build_context();
            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: if result.scrollable {
                    coderex_ui::message_list::MessageRole::Assistant
                } else {
                    coderex_ui::message_list::MessageRole::System
                },
                content: result.output.into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        } else {
            // Non-command string: treat as a user message
            ui_state.messages.push(coderex_ui::message_list::Message {
                role: coderex_ui::message_list::MessageRole::System,
                content: format!("[Macro] {trimmed}").into(),
                tool_name: None,
                streaming: false,
                folded: false,
            });
        }
    }

    async fn handle_input(&mut self, line: &str, stdout: &mut impl Write) -> std::io::Result<()> {
        if line.starts_with('/') {
            self.execute_command(line, stdout).await?;
        } else if line.starts_with('#') {
            // Comment line, ignore
        } else {
            // 14.1: Route to QueryEngine instead of placeholder
            if self.engine.is_some() {
                self.history.push(line.to_string());
                let output = self.submit_prompt(line, false).await;
                writeln!(stdout, "{output}")?;
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "No QueryEngine configured. Set ANTHROPIC_API_KEY or run `coderex --api-key <key>`",
                ));
            }
        }
        Ok(())
    }

    async fn execute_command(
        &mut self,
        line: &str,
        stdout: &mut impl Write,
    ) -> std::io::Result<()> {
        // Check for pipe operator
        if line.contains('|') {
            return self.execute_piped_commands_non_tui(line, stdout);
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd_name = parts[0].trim_start_matches('/');
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        // Check for macro expansion
        if self.macro_registry.has(cmd_name) {
            if let Some(commands) = self.macro_registry.get(cmd_name) {
                let cmds: Vec<String> = commands.to_vec();
                for macro_cmd in cmds {
                    self.execute_single_command_non_tui(&macro_cmd, stdout)?;
                }
                return Ok(());
            }
        }

        // Built-in REPL commands that don't go through registry
        match cmd_name {
            "exit" | "quit" => {
                self.running = false;
                writeln!(stdout, "Exiting...")?;
                return Ok(());
            }
            "help" => {
                self.show_help(stdout)?;
                return Ok(());
            }
            "cancel" => {
                self.cancel_background_tasks();
                writeln!(stdout, "All background tasks cancelled.")?;
                return Ok(());
            }
            "compact" => {
                let msg = self.compact_context_non_tui().await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "agent-summary" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::AgentSummary,
                    "Summarize what the agent has done recently.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "magic-docs" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::MagicDocs,
                    "Generate documentation for the current codebase.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "generate-readme" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::ReadmeGeneration,
                    "Generate a README for the current project.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "away-summary" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::AwaySummary,
                    "Summarize what happened while the user was away.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "codebase-overview" => {
                let msg = self.spawn_background_feature_by_kind(
                    BackgroundFeatureKind::CodebaseOverview,
                    "Generate a high-level overview of the codebase.".into(),
                );
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "update-memory" => {
                let msg = self.spawn_memory_update_task().await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "extract-memories" => {
                let msg = self.spawn_memory_extraction_task().await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "agent-generation" => {
                let description = args.join(" ");
                if description.is_empty() {
                    writeln!(stdout, "Usage: /agent-generation <description>")?;
                } else {
                    let msg = self.run_agent_generation(&description).await;
                    writeln!(stdout, "{msg}")?;
                }
                return Ok(());
            }
            "auto-dream" => {
                let msg = self.handle_auto_dream_command(&args);
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "team-memory" => {
                let msg = self.handle_team_memory_command(&args).await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "workspace" => {
                let msg = self.handle_workspace_command(&args);
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "recover" => {
                let msg = self.handle_recover_command();
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "update-check" => {
                let msg = self.handle_update_check_command().await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "feedback" => {
                let msg = self.handle_feedback_command(&args).await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "teleport" => {
                let msg = self.handle_teleport_command(&args).await;
                writeln!(stdout, "{msg}")?;
                return Ok(());
            }
            "clear" => {
                if let Some(ref mut engine) = self.engine {
                    let mut session = engine.session().clone();
                    session.messages.clear();
                    engine.set_session(session);
                }
                self.history.clear();
                self.session_effects = SessionEffects::default();
                writeln!(stdout, "Session cleared.")?;
                return Ok(());
            }
            "resume" => {
                let session_id = if args.is_empty() {
                    session_resume::latest_session_id(&self.cwd).unwrap_or_default()
                } else {
                    args[0].clone()
                };
                if session_id.is_empty() {
                    writeln!(stdout, "No previous sessions found.")?;
                } else {
                    match self.load_session_state(&session_id, true) {
                        Ok((messages, _)) => {
                            writeln!(
                                stdout,
                                "Resumed session: {session_id} ({} messages)",
                                messages.len()
                            )?;
                        }
                        Err(e) => {
                            writeln!(stdout, "Failed to resume session: {e}")?;
                        }
                    }
                }
                return Ok(());
            }
            "rename" => {
                if args.is_empty() {
                    writeln!(stdout, "Usage: /rename <new-title>")?;
                } else {
                    let new_title = args.join(" ");
                    if let Some(ref mut engine) = self.engine {
                        let mut session = engine.session().clone();
                        session.title = Some(new_title.clone());
                        engine.set_session(session);
                    }
                    writeln!(stdout, "Session renamed to: '{new_title}'")?;
                }
                return Ok(());
            }
            "rewind" => {
                if args.is_empty() {
                    writeln!(stdout, "Usage: /rewind <message-number>")?;
                } else {
                    match args[0].parse::<usize>() {
                        Ok(n) => {
                            if let Some(ref mut engine) = self.engine {
                                let mut session = engine.session().clone();
                                if n < session.messages.len() {
                                    let removed = session.messages.len() - n;
                                    session.messages.truncate(n);
                                    engine.set_session(session);
                                    self.history.truncate(n);
                                    writeln!(
                                        stdout,
                                        "Rewound to message {n}. {removed} messages removed."
                                    )?;
                                } else {
                                    writeln!(
                                        stdout,
                                        "Message {n} not found. Session has {} messages.",
                                        session.messages.len()
                                    )?;
                                }
                            } else {
                                writeln!(stdout, "No engine configured.")?;
                            }
                        }
                        Err(_) => {
                            writeln!(stdout, "Invalid message number.")?;
                        }
                    }
                }
                return Ok(());
            }
            "model" => {
                if args.is_empty() {
                    writeln!(stdout, "Current model: {}", self.model.0)?;
                } else {
                    let model_name = args[0].clone();
                    self.model = ModelId(model_name.clone());
                    writeln!(stdout, "Model switched to: {model_name}")?;
                }
                return Ok(());
            }
            "permissions" => {
                if args.is_empty() {
                    let mode = if let Some(ref engine) = self.engine {
                        engine
                            .permission_checker()
                            .lock()
                            .map(|c| c.mode())
                            .unwrap_or_default()
                    } else {
                        self.permission_mode.unwrap_or_default()
                    };
                    writeln!(stdout, "Current permission mode: {mode:?}")?;
                } else {
                    let mode = match args[0].to_lowercase().as_str() {
                        "default" => PermissionMode::Default,
                        "acceptedits" | "accept_edits" => PermissionMode::AcceptEdits,
                        "bypasspermissions" | "bypass" | "bypass_permissions" => {
                            PermissionMode::BypassPermissions
                        }
                        "dontask" | "dont_ask" => PermissionMode::DontAsk,
                        "plan" | "planmode" | "plan_mode" => PermissionMode::Plan,
                        "auto" => PermissionMode::Auto,
                        _ => {
                            writeln!(
                                stdout,
                                "Unknown mode: {}. Available: default, acceptedits, bypasspermissions, dontask, plan, auto",
                                args[0]
                            )?;
                            return Ok(());
                        }
                    };
                    if let Some(ref engine) = self.engine {
                        if let Ok(mut checker) = engine.permission_checker().lock() {
                            checker.set_mode(mode);
                        }
                    }
                    self.permission_mode = Some(mode);
                    writeln!(stdout, "Permission mode set to: {mode:?}")?;
                }
                return Ok(());
            }
            "break-cache" => {
                if let Some(ref mut engine) = self.engine {
                    let mut session = engine.session().clone();
                    let previous = session.usage.cache_read_tokens;
                    session.usage.cache_read_tokens = 0;
                    session.usage.cache_creation_tokens = 0;
                    engine.set_session(session);
                    writeln!(
                        stdout,
                        "Prompt cache cleared. Previous cache: {previous} tokens"
                    )?;
                } else {
                    writeln!(stdout, "No engine configured.")?;
                }
                return Ok(());
            }
            "diff" => {
                let dir = std::path::Path::new(&self.cwd);
                let diff = std::process::Command::new("git")
                    .args(["diff"])
                    .current_dir(dir)
                    .output();
                match diff {
                    Ok(output) if output.status.success() => {
                        let text = String::from_utf8_lossy(&output.stdout);
                        if text.is_empty() {
                            writeln!(stdout, "No changes.")?;
                        } else {
                            writeln!(stdout, "Git Diff\n────────\n{text}")?;
                        }
                    }
                    _ => {
                        writeln!(stdout, "No changes or not a git repository.")?;
                    }
                }
                return Ok(());
            }
            _ => {}
        }

        self.command_history.append(cmd_name, &args);
        let ctx = self.build_context();
        let result = self.commands.execute(cmd_name, &ctx, &args);
        self.sync_command_state(cmd_name, &result);
        writeln!(stdout, "{}", result.output)?;
        Ok(())
    }

    fn show_help(&self, stdout: &mut impl Write) -> std::io::Result<()> {
        writeln!(stdout, "Available commands:")?;
        for cmd in self.commands.list() {
            writeln!(stdout, "  /{:<20} {}", cmd.name, cmd.description)?;
        }
        Ok(())
    }

    /// Execute piped commands in non-TUI mode.
    fn execute_piped_commands_non_tui(
        &mut self,
        line: &str,
        stdout: &mut impl Write,
    ) -> std::io::Result<()> {
        let segments: Vec<&str> = line.trim_start_matches('/').split('|').collect();
        let mut pipe_input: Option<String> = None;

        for segment in &segments {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            let parts: Vec<&str> = segment.split_whitespace().collect();
            let cmd_name = parts[0];
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

            self.command_history.append(cmd_name, &args);

            let mut ctx = self.build_context();
            ctx.pipe_input = pipe_input;

            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);
            writeln!(stdout, "{}", result.output)?;
            pipe_input = Some(result.output);
        }
        Ok(())
    }

    /// Execute a single command string in non-TUI mode (from macro expansion).
    fn execute_single_command_non_tui(
        &mut self,
        cmd_str: &str,
        stdout: &mut impl Write,
    ) -> std::io::Result<()> {
        let trimmed = cmd_str.trim();
        if trimmed.starts_with('/') {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            let cmd_name = parts[0].trim_start_matches('/');
            let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
            self.command_history.append(cmd_name, &args);
            let ctx = self.build_context();
            let result = self.commands.execute(cmd_name, &ctx, &args);
            self.sync_command_state(cmd_name, &result);
            writeln!(stdout, "{}", result.output)?;
        } else {
            writeln!(stdout, "[Macro] {trimmed}")?;
        }
        Ok(())
    }

    /// Handle /auto-dream command.
    fn handle_auto_dream_command(&mut self, args: &[String]) -> String {
        let dream = match &self.auto_dream {
            Some(d) => d,
            None => return "Auto-dream is not initialized.".into(),
        };

        if args.is_empty() {
            return "Usage: /auto-dream <enable|disable|status>".into();
        }

        match args[0].as_str() {
            "enable" => {
                let mut config = dream.config.clone();
                config.enabled = true;
                if args.len() >= 2 {
                    if let Ok(n) = args[1].parse::<usize>() {
                        config.session_threshold = n;
                    }
                }
                if args.len() >= 3 {
                    if let Ok(n) = args[2].parse::<i64>() {
                        config.time_threshold_hours = n;
                    }
                }
                // Record a session to trigger immediate check if threshold is 1
                let _ = dream.record_session();

                // Spawn the watcher if API client is available
                if let Some(api) = &self.api_client {
                    let dream = dream.clone();
                    let api = api.clone_box();
                    let model = self.model.clone();
                    let _memory_dir = dream.memory_dir.clone();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    self.dream_shutdown_tx = Some(shutdown_tx);

                    tokio::spawn(async move {
                        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
                        let watcher_handle = tokio::spawn(async move {
                            // Collect recent session summaries as context
                            let recent_sessions: Vec<String> = Vec::new();
                            dream
                                .spawn_watcher_with_model(
                                    tx,
                                    std::time::Duration::from_secs(30),
                                    shutdown_rx,
                                    api.as_ref(),
                                    &model,
                                    recent_sessions,
                                )
                                .await;
                        });

                        // Forward dream results to memory dir
                        while let Some(session) = rx.recv().await {
                            tracing::info!(
                                "Auto-dream completed: analyzed {} sessions",
                                session.sessions_analyzed
                            );
                            // Dream artifacts are already written by complete_dream
                            let _ = session;
                        }
                        let _ = watcher_handle.await;
                    });

                    format!(
                        "Auto-dream enabled (threshold: {} sessions or {}h)",
                        config.session_threshold, config.time_threshold_hours
                    )
                } else {
                    "Auto-dream requires an API client and memory store.".into()
                }
            }
            "disable" => {
                if let Some(tx) = self.dream_shutdown_tx.take() {
                    let _ = tx.send(());
                    "Auto-dream disabled.".into()
                } else {
                    "Auto-dream is not running.".into()
                }
            }
            "status" => {
                let config = &dream.config;
                let running = self.dream_shutdown_tx.is_some();
                let threshold = dream.check_threshold();
                let threshold_msg = match threshold {
                    DreamCheckResult::ShouldDream {
                        sessions_since_last,
                        hours_since_last,
                    } => format!(
                        "thresholds met: {sessions_since_last} sessions, {hours_since_last}h since last dream"
                    ),
                    DreamCheckResult::NotReady => "thresholds not yet met".into(),
                    DreamCheckResult::InProgress => "dream in progress".into(),
                };
                format!(
                    "Auto-dream: {} | config: {} sessions or {}h | {threshold_msg}",
                    if running { "running" } else { "stopped" },
                    config.session_threshold,
                    config.time_threshold_hours,
                )
            }
            _ => "Usage: /auto-dream <enable|disable|status>".into(),
        }
    }

    /// Handle /team-memory command.
    async fn handle_team_memory_command(&mut self, args: &[String]) -> String {
        let svc = match &self.team_memory_service {
            Some(s) => s,
            None => return "Team memory sync is not initialized.".into(),
        };

        if args.is_empty() {
            return "Usage: /team-memory <upsert|read|list|delete|sync> [args...]".into();
        }

        match args[0].as_str() {
            "upsert" => {
                if args.len() < 3 {
                    return "Usage: /team-memory upsert <topic> <content>".into();
                }
                let topic = &args[1];
                let content = args[2..].join(" ");
                match svc.upsert(topic, &content) {
                    Ok(entry) => format!(
                        "Team memory upserted: '{}' (checksum: {})",
                        entry.topic,
                        entry.checksum.to_hex()
                    ),
                    Err(e) => format!("Failed to upsert team memory: {e}"),
                }
            }
            "read" => {
                if args.len() < 2 {
                    return "Usage: /team-memory read <topic>".into();
                }
                match svc.read(&args[1]) {
                    Ok(Some(entry)) => format!("[{}]\n{}", entry.topic, entry.content),
                    Ok(None) => "No entry found.".into(),
                    Err(e) => format!("Failed to read team memory: {e}"),
                }
            }
            "list" => match svc.list() {
                Ok(entries) if entries.is_empty() => "No team memories.".into(),
                Ok(entries) => entries
                    .iter()
                    .map(|e| {
                        format!(
                            "- {} (updated: {})",
                            e.topic,
                            e.updated_at.format("%Y-%m-%d %H:%M")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(e) => format!("Failed to list team memories: {e}"),
            },
            "delete" => {
                if args.len() < 2 {
                    return "Usage: /team-memory delete <topic>".into();
                }
                match svc.delete(&args[1]) {
                    Ok(()) => "Team memory deleted.".into(),
                    Err(e) => format!("Failed to delete team memory: {e}"),
                }
            }
            "sync" => {
                // Sync via in-memory relay or HTTP transport
                // For now, show local state info
                match svc.list() {
                    Ok(entries) => {
                        format!(
                            "Local team memories: {} entries. Use /team-memory sync --remote to sync with relay.",
                            entries.len()
                        )
                    }
                    Err(e) => format!("Failed to list for sync: {e}"),
                }
            }
            _ => "Usage: /team-memory <upsert|read|list|delete|sync> [args...]".into(),
        }
    }

    /// Handle /workspace command — switch or show current workspace directory.
    fn handle_workspace_command(&mut self, args: &[String]) -> String {
        if args.is_empty() {
            return format!("Current workspace: {}", self.cwd);
        }
        let path = &args[0];
        let expanded = if path.starts_with('~') {
            if let Some(home) = home::home_dir() {
                home.join(&path[1..]).to_string_lossy().to_string()
            } else {
                return "Could not expand ~ path.".into();
            }
        } else {
            path.to_string()
        };
        let target = std::path::Path::new(&expanded);
        if !target.exists() {
            return format!("Workspace path does not exist: {expanded}");
        }
        if !target.is_dir() {
            return format!("Path is not a directory: {expanded}");
        }
        let abs = target.canonicalize().unwrap_or(target.to_path_buf());
        let old = std::mem::replace(&mut self.cwd, abs.to_string_lossy().to_string());
        format!("Workspace switched: {old} → {}", self.cwd)
    }

    /// Handle /recover command — restore session from crash recovery snapshot.
    fn handle_recover_command(&self) -> String {
        let Some(state) = crate::recovery::load(&self.cwd) else {
            return "No recovery snapshot found.".into();
        };
        format!(
            "Recovery snapshot found: session={}, messages={}, last_input='{}', saved_at={}",
            state.session_id, state.message_count, state.last_input, state.saved_at
        )
    }

    /// Handle /update-check command — check for new versions.
    async fn handle_update_check_command(&self) -> String {
        match crate::version::check_for_updates().await {
            Some(msg) => msg,
            None => format!(
                "You are running the latest version ({})",
                crate::version::CURRENT_VERSION
            ),
        }
    }

    /// Handle /feedback command — generate a title for user feedback.
    async fn handle_feedback_command(&self, args: &[String]) -> String {
        let text = args.join(" ");
        if text.is_empty() {
            return "Usage: /feedback <message> — generates a title for your feedback.".into();
        }
        let Some(ref client) = self.api_client else {
            return "No API client available.".into();
        };
        match coderex_core::prompts::generate_session_title(client.as_ref(), &text).await {
            Ok(title) => format!("Feedback title: \"{title}\"\n\nFeedback: {text}"),
            Err(e) => format!("Failed to generate title: {e}"),
        }
    }

    /// Handle the /teleport command: directory switching, share, join, list, revoke.
    async fn handle_teleport_command(&mut self, args: &[String]) -> String {
        let mgr = match self.teleport_manager {
            Some(ref mgr) => mgr.clone(),
            None => return "Teleport manager not initialized.".into(),
        };

        if args.is_empty() {
            return "Usage: /teleport <directory> | --share | --join <token> | --list | --revoke <token>".into();
        }

        match args[0].as_str() {
            "--share" => {
                let session = self.engine.as_ref().map(|e| e.session().clone());
                match session {
                    Some(session) => match mgr.share_session(&session) {
                        Ok(token) => format!("Share token: {token}"),
                        Err(e) => format!("Failed to share session: {e}"),
                    },
                    None => "No active session to share.".into(),
                }
            }
            "--join" => {
                if args.len() < 2 {
                    return "Usage: /teleport --join <token>".into();
                }
                let token = &args[1];
                match mgr.join_session(token) {
                    Ok(state) => {
                        let new_cwd = state.cwd.to_string_lossy().to_string();
                        self.cwd = new_cwd.clone();
                        let _ = std::env::set_current_dir(&new_cwd);
                        if let Some(ref mut engine) = self.engine {
                            engine.set_session(state);
                        }
                        format!("Joined shared session. CWD: {new_cwd}")
                    }
                    Err(e) => format!("Failed to join session: {e}"),
                }
            }
            "--list" => {
                let shares = mgr.list_shares();
                if shares.is_empty() {
                    "No active shares.".into()
                } else {
                    format!("Active shares: {}", shares.join(", "))
                }
            }
            "--revoke" => {
                if args.len() < 2 {
                    return "Usage: /teleport --revoke <token>".into();
                }
                let token = &args[1];
                match mgr.revoke_share(token) {
                    Ok(true) => format!("Revoked share: {token}"),
                    Ok(false) => format!("Share not found: {token}"),
                    Err(e) => format!("Failed to revoke share: {e}"),
                }
            }
            dir => {
                let target_dir = std::path::PathBuf::from(dir);
                if !target_dir.exists() {
                    return format!("Directory does not exist: {dir}");
                }
                if !target_dir.is_dir() {
                    return format!("Not a directory: {dir}");
                }

                // Save current session to current directory
                if let Some(ref engine) = self.engine {
                    let session = engine.session().clone();
                    if let Err(e) = mgr.save_to_directory(&session, std::path::Path::new(&self.cwd))
                    {
                        return format!("Failed to save session: {e}");
                    }
                }

                // Try to load session from target directory
                let loaded = match mgr.load_from_directory(&target_dir) {
                    Ok(state) => state,
                    Err(e) => return format!("Failed to load session: {e}"),
                };

                // Update CWD and process working directory
                let new_cwd = target_dir
                    .canonicalize()
                    .unwrap_or(target_dir)
                    .to_string_lossy()
                    .to_string();
                self.cwd = new_cwd.clone();
                let _ = std::env::set_current_dir(&new_cwd);

                if let Some(ref mut engine) = self.engine {
                    if let Some(state) = loaded {
                        engine.set_session(state);
                    } else {
                        let mut fresh = SessionState::new(std::path::PathBuf::from(&new_cwd));
                        fresh.session_id = engine.session().session_id;
                        engine.set_session(fresh);
                    }
                }

                format!("Teleported to: {new_cwd}")
            }
        }
    }

    fn build_context(&self) -> CommandContext {
        let (session_id, session_title, message_count, permission_mode) =
            if let Some(ref engine) = self.engine {
                let session = engine.session();
                let permission_mode = engine
                    .permission_checker()
                    .lock()
                    .map(|checker| checker.mode())
                    .unwrap_or_default();
                (
                    session.session_id.to_string(),
                    session
                        .title
                        .clone()
                        .unwrap_or_else(|| "REPL Session".to_string()),
                    session.messages.len(),
                    permission_mode,
                )
            } else {
                (
                    "repl_session".to_string(),
                    "REPL Session".to_string(),
                    self.history.len(),
                    PermissionMode::default(),
                )
            };

        let mut ctx = CommandContext::new(
            self.cwd.clone(),
            session_id,
            session_title,
            self.model.clone(),
        );
        ctx.message_count = message_count;
        ctx.permission_mode = permission_mode;
        ctx.vim_mode = self.vim_mode_enabled;
        ctx.theme = self.current_theme.clone();
        ctx.git_branch = current_git_branch(&self.cwd);
        ctx.version = env!("CARGO_PKG_VERSION").to_string();
        ctx.feature_flags = HashMap::new();
        ctx.api_key_set = self.api_client.is_some();
        ctx.elapsed_secs = self
            .query_start_time
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        if let Some(ref engine) = self.engine {
            ctx.cost_summary = engine.cost_summary();
            let usage = engine.total_usage();
            ctx.input_tokens = usage.input_tokens;
            ctx.output_tokens = usage.output_tokens;
            ctx.cache_read_tokens = usage.cache_read_tokens;
            ctx.cache_creation_tokens = usage.cache_creation_tokens;
            ctx.cost_usd = engine.session().cost_usd;
        }
        ctx
    }
}

/// Get the current git branch for the statusline.
fn current_git_branch(cwd: &str) -> String {
    std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Session state for the REPL.
#[derive(Debug)]
pub struct ReplSession {
    /// The session ID.
    pub id: String,
    /// The session title.
    pub title: String,
    /// Whether the session is active.
    pub active: bool,
    /// Message count.
    pub message_count: usize,
}

impl ReplSession {
    /// Create a new session.
    #[must_use]
    pub fn new(id: String, title: String) -> Self {
        Self {
            id,
            title,
            active: true,
            message_count: 0,
        }
    }
}

/// Parse a line of input into a command and arguments.
#[must_use]
pub fn parse_input(line: &str) -> Option<(&str, Vec<String>)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('/') {
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        let cmd = parts[0].trim_start_matches('/');
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        Some((cmd, args))
    } else {
        Some(("message", vec![trimmed.to_string()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::api_client::{ApiError, ApiResponse, StreamEvent};
    use coderex_core::tool_executor::{
        ToolDispatchOutcome, ToolExecutor as CoreToolExecutor, ToolInfo,
    };
    use coderex_core::types::{ToolResult, UsageSummary};

    #[derive(Clone)]
    struct StubToolExecutor {
        tools: Vec<ToolInfo>,
    }

    #[async_trait::async_trait]
    impl CoreToolExecutor for StubToolExecutor {
        fn list_tools(&self) -> Vec<ToolInfo> {
            self.tools.clone()
        }

        async fn execute(
            &self,
            tool_call_id: &str,
            _tool_name: &str,
            _input: &serde_json::Value,
        ) -> ToolDispatchOutcome {
            ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: "ok".into(),
                    is_error: false,
                },
            }
        }
    }

    #[derive(Clone)]
    struct FixedResultToolExecutor {
        output: String,
        is_error: bool,
    }

    #[async_trait::async_trait]
    impl CoreToolExecutor for FixedResultToolExecutor {
        async fn execute(
            &self,
            tool_call_id: &str,
            _tool_name: &str,
            _input: &serde_json::Value,
        ) -> ToolDispatchOutcome {
            ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: self.output.clone(),
                    is_error: self.is_error,
                },
            }
        }
    }

    /// Two-turn API double: the sub-agent must read `note.txt` with the real
    /// tool dispatcher before it can produce the successful final answer.
    #[derive(Clone)]
    struct ReadThenAnswerApi {
        call: Arc<std::sync::Mutex<usize>>,
        expected_contents: &'static str,
    }

    #[async_trait::async_trait]
    impl ApiClient for ReadThenAnswerApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ApiError> {
            let call = {
                let mut call = self.call.lock().unwrap();
                *call += 1;
                *call
            };
            let usage = UsageSummary {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            if call == 1 {
                return Ok(ApiResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "child_read".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "note.txt"}),
                    }],
                    usage,
                    stop_reason: "tool_use".into(),
                });
            }

            let saw_tool_output = messages
                .iter()
                .any(|message| message.to_string().contains(self.expected_contents));
            Ok(ApiResponse {
                content: if saw_tool_output {
                    "sub-agent read the file"
                } else {
                    "sub-agent could not read the file"
                }
                .into(),
                tool_calls: Vec::new(),
                usage,
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_input("").is_none());
        assert!(parse_input("   ").is_none());
    }

    #[test]
    fn parse_command() {
        let result = parse_input("/clear").unwrap();
        assert_eq!(result.0, "clear");
        assert!(result.1.is_empty());
    }

    #[test]
    fn parse_command_with_args() {
        let result = parse_input("/model sonnet").unwrap();
        assert_eq!(result.0, "model");
        assert_eq!(result.1, vec!["sonnet"]);
    }

    #[test]
    fn parse_message() {
        let result = parse_input("Hello, world!").unwrap();
        assert_eq!(result.0, "message");
        assert_eq!(result.1, vec!["Hello, world!"]);
    }

    #[test]
    fn parse_command_multiple_args() {
        let result = parse_input("/btw this is important").unwrap();
        assert_eq!(result.0, "btw");
        assert_eq!(result.1, vec!["this", "is", "important"]);
    }

    #[test]
    fn repl_loop_new() {
        let loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        assert!(!loop_obj.commands.is_empty());
    }

    #[test]
    fn arc_tool_executor_forwards_list_tools() {
        let tool = ToolInfo {
            name: "Read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type":"object"}),
        };
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> =
            Arc::new(StubToolExecutor { tools: vec![tool] });
        let wrapped = ArcToolExecutor(inner);
        let tools = wrapped.list_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Read");
    }

    #[test]
    fn plugin_fallback_executor_advertises_primary_tools() {
        let tool = ToolInfo {
            name: "Read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type":"object"}),
        };
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> =
            Arc::new(StubToolExecutor { tools: vec![tool] });
        let primary = ArcToolExecutor(inner);
        let plugin = Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
            Arc::new(coderex_plugins::plugins::PluginManager::new()),
        ));
        let wrapped = PluginToolFallbackExecutor { primary, plugin };
        let tools = wrapped.list_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Read");
    }

    #[tokio::test]
    async fn plugin_fallback_preserves_primary_tool_errors_that_mention_not_found() {
        let primary_output =
            "Skill 'frontend-design' not found.\nAvailable skills: pdf".to_string();
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> = Arc::new(FixedResultToolExecutor {
            output: primary_output.clone(),
            is_error: true,
        });
        let primary = ArcToolExecutor(inner);
        let plugin = Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
            Arc::new(coderex_plugins::plugins::PluginManager::new()),
        ));
        let wrapped = PluginToolFallbackExecutor { primary, plugin };

        let result = wrapped
            .execute(
                "tool_1",
                "Skill",
                &serde_json::json!({"skill":"frontend-design"}),
            )
            .await
            .result;

        assert_eq!(result.output, primary_output);
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn plugin_fallback_runs_only_when_primary_tool_is_missing() {
        let inner: Arc<dyn CoreToolExecutor + Send + Sync> = Arc::new(FixedResultToolExecutor {
            output: "tool 'PluginCommand' not found in registry".into(),
            is_error: true,
        });
        let primary = ArcToolExecutor(inner);
        let plugin = Arc::new(coderex_plugins::tool_executor::PluginToolExecutor::new(
            Arc::new(coderex_plugins::plugins::PluginManager::new()),
        ));
        let wrapped = PluginToolFallbackExecutor { primary, plugin };

        let result = wrapped
            .execute("tool_1", "PluginCommand", &serde_json::json!({}))
            .await
            .result;

        assert_eq!(result.output, "Plugin tool 'PluginCommand' not found");
        assert!(result.is_error);
    }

    #[test]
    fn repl_session_new() {
        let session = ReplSession::new("s1".into(), "Test".into());
        assert_eq!(session.id, "s1");
        assert!(session.active);
        assert_eq!(session.message_count, 0);
    }

    #[tokio::test]
    async fn headless_command() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("/status").await.unwrap();
        assert!(result.contains("System Status"));
    }

    #[tokio::test]
    async fn headless_message_without_engine() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("Hello").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("QueryEngine"),
            "expected QueryEngine in error: {err}"
        );
    }

    #[tokio::test]
    async fn headless_help() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = loop_obj.run_headless("/help").await.unwrap();
        assert!(result.contains("System Status") || result.contains("Available"));
    }

    #[test]
    fn normalize_headless_read_output_deduplicates_repeated_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("lib.rs");
        let content = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        std::fs::write(&file, content).unwrap();

        let loop_obj = ReplLoop::new(dir.path().to_string_lossy().to_string(), ModelId::sonnet());
        let raw = format!(
            "I'll read the file first.\n<function_calls>...\n{content}Functions:\n- add\n{content}"
        );
        let normalized =
            loop_obj.normalize_headless_read_output("Read src/lib.rs and summarize it", raw);

        assert_eq!(normalized.matches(content.trim()).count(), 1);
        assert!(normalized.contains("Functions:\n- add"));
        assert!(normalized.starts_with("pub fn add"));
    }

    #[test]
    fn session_effects_default() {
        let effects = SessionEffects::default();
        assert!(effects.prompt_profiles.is_empty());
        assert!(effects.append_system_prompt.is_none());
        assert!(effects.allowed_tools_override.is_none());
        assert!(effects.extra_context.is_empty());
        assert!(!effects.title_generated);
    }

    #[test]
    fn apply_command_effects_from_profile() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        let result = CommandResult::success("advisor mode")
            .with_prompt_profile(coderex_core::query_profiles::PromptProfile::Advisor);
        loop_obj.apply_command_effects(&result);
        assert_eq!(loop_obj.session_effects.prompt_profiles.len(), 1);
        assert_eq!(
            loop_obj.session_effects.prompt_profiles[0],
            coderex_core::query_profiles::PromptProfile::Advisor
        );
    }

    #[test]
    fn build_submit_options_includes_effects() {
        let mut loop_obj = ReplLoop::new("/src".into(), ModelId::sonnet());
        loop_obj.session_effects.prompt_profiles = vec![
            coderex_core::query_profiles::PromptProfile::Advisor,
            coderex_core::query_profiles::PromptProfile::Reviewer,
        ];
        let options = loop_obj.build_submit_options();
        assert!(options.custom_system_prompt.is_some());
        let prompt = options.custom_system_prompt.unwrap();
        assert!(prompt.contains("senior technical advisor"));
        assert!(prompt.contains("code review specialist"));
        assert_eq!(
            options.allowed_tools_override.unwrap(),
            vec!["Read", "Grep"]
        );
        assert!(!options.handle_elicitation);
    }

    #[test]
    fn current_git_branch_falls_back() {
        let branch = current_git_branch("/nonexistent");
        assert!(branch.is_empty());
    }

    #[test]
    fn interactive_phase_labels_track_activity() {
        let mut phase = InteractivePhase::Planning;
        assert_eq!(phase.status_label().as_deref(), Some("Planning..."));

        phase.note_text_delta();
        assert_eq!(phase.status_label().as_deref(), Some("Thinking..."));

        phase.note_tool_start("Running cargo test -p coderex-cli...");
        assert_eq!(
            phase.status_label().as_deref(),
            Some("Running cargo test -p coderex-cli...")
        );

        phase.note_permission_request("Read", &serde_json::json!({"file_path":"src/lib.rs"}));
        assert_eq!(
            phase.status_label().as_deref(),
            Some("Waiting for permission: Reading src/lib.rs...")
        );

        phase.note_tool_end();
        assert_eq!(phase.status_label().as_deref(), Some("Thinking..."));

        phase.reset();
        assert!(phase.status_label().is_none());
    }

    #[test]
    fn permission_activity_labels_are_user_facing() {
        assert_eq!(
            permission_activity_label("Running cargo test -p coderex-cli..."),
            "Waiting for permission: Running cargo test -p coderex-cli..."
        );
    }

    #[tokio::test]
    async fn agent_tool_runs_a_real_subagent_with_parent_tools_and_cwd() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("note.txt"), "needle-from-child").unwrap();
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "needle-from-child",
        };
        let mut repl = ReplLoop::with_engine(
            Box::new(api),
            temp.path().to_string_lossy().into_owned(),
            ModelId::sonnet(),
        );
        let dispatcher = repl.dispatcher.take().unwrap();
        let manager = repl.agent_manager.take().unwrap();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            dispatcher.execute(
                "parent_agent_call",
                "Agent",
                &serde_json::json!({
                    "description": "read a local note",
                    "prompt": "Read note.txt and report its contents"
                }),
            ),
        )
        .await
        .expect("sub-agent did not complete");

        assert!(
            !outcome.result.is_error,
            "agent tool failed: {}",
            outcome.result.output
        );
        let payload: serde_json::Value = serde_json::from_str(&outcome.result.output).unwrap();
        assert_eq!(payload["status"], "Completed");
        assert_eq!(payload["output"], "sub-agent read the file");
        let output_file = payload["output_file"].as_str().unwrap();
        assert!(
            std::path::Path::new(output_file).is_file(),
            "missing sub-agent output: {output_file}"
        );

        let manager = manager.lock().await;
        assert_eq!(manager.list_agents().len(), 1);
        assert_eq!(
            manager.list_agents()[0].status(),
            coderex_agents::AgentStatus::Completed
        );
        drop(manager);

        let mut saw_started = false;
        let mut saw_completed = false;
        while let Ok(event) = repl.event_rx.as_mut().unwrap().try_recv() {
            if let EngineEvent::AgentEvent { event, .. } = event {
                match event {
                    coderex_core::types::AgentLifecycleEvent::Started => saw_started = true,
                    coderex_core::types::AgentLifecycleEvent::Completed { .. } => {
                        saw_completed = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_started, "missing sub-agent start event");
        assert!(saw_completed, "missing sub-agent completion event");
    }

    #[tokio::test]
    async fn artifact_tools_join_the_registry_and_serve_a_real_page() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());
        let store = tempfile::tempdir().unwrap();
        repl.with_artifact_store_root(store.path().to_path_buf());

        repl.start_artifact_serving().await;
        assert!(
            repl.artifact_server.is_some(),
            "the artifact server did not start"
        );

        let dispatcher = repl.dispatcher.take().unwrap();
        let registry = dispatcher.registry();
        for name in [
            "Artifact",
            "ArtifactData",
            "ArtifactComments",
            "ArtifactCheck",
        ] {
            assert!(
                registry.get_tool_arc(name).is_some(),
                "{name} was not registered"
            );
        }

        // Publishing through the registered tool produces a page the server
        // actually answers for.
        let tool = registry.get_tool_arc("Artifact").unwrap();
        let output = tool
            .execute(
                coderex_tools::ToolInput {
                    call_id: "call_1".into(),
                    name: "Artifact".into(),
                    arguments: serde_json::json!({
                        "title": "Session report",
                        "files": { "index.html": "<html><body>hello</body></html>" }
                    }),
                },
                coderex_tools::ToolContext::new(
                    coderex_tools::ExecutionContext::Normal,
                    std::path::Path::new("/tmp"),
                ),
            )
            .await
            .unwrap();

        let url = output
            .output
            .lines()
            .next()
            .unwrap()
            .split('(')
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .to_string();
        let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(body.contains("hello"), "{body}");

        if let Some(server) = repl.artifact_server.take() {
            server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn mcp_tools_join_the_registry_the_model_dispatches_through() {
        let api = ReadThenAnswerApi {
            call: Arc::new(std::sync::Mutex::new(0)),
            expected_contents: "unused",
        };
        let mut repl = ReplLoop::with_engine(Box::new(api), "/tmp".into(), ModelId::sonnet());

        let manager = Arc::new(coderex_mcp::manager::McpManager::new());
        manager.register_tool(coderex_mcp::proxy::McpToolInfo {
            server: "mock".into(),
            name: "ping".into(),
            description: "Ping the mock server".into(),
            input_schema: serde_json::json!({"type": "object"}),
        });

        repl.with_mcp_manager(Arc::clone(&manager));

        let dispatcher = repl.dispatcher.take().unwrap();
        let registry = dispatcher.registry();
        assert!(
            registry.get_tool_arc("mcp__mock__ping").is_some(),
            "MCP tool did not reach the registry the model dispatches through"
        );
        assert!(
            registry.get_tool_arc("Bash").is_some(),
            "built-in tools disappeared"
        );
    }
}
