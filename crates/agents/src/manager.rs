//! AgentManager — spawn, manage, and communicate with sub-agents.
//!
//! Phase 10.3 + 10.5: Provides agent spawning with context inheritance,
//! tool whitelisting, messaging, worktree isolation, and teammate events.

use coderex_core::api_client::ApiClient;
use coderex_core::confirmation_bus::ConfirmationBus;
use coderex_core::elicitation::SdkElicitationHandler;
use coderex_core::engine::{
    QueryEngine, StructuredOutputValidator, SubmitOptions, ValidationResult,
};
use coderex_core::orchestrator::{
    BackgroundFeatureRequest, QueryResult, run_background_feature, run_forked_agent,
};
use coderex_core::permission_engine::PermissionEngine;
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::sandbox::SandboxConfig;
use coderex_core::state::SessionState;
use coderex_core::task::{TaskHandle, TaskKind, TaskManager};
use coderex_core::tool_executor::ToolExecutor;
use coderex_core::types::{AgentLifecycleEvent, EngineEvent, ModelId};
use coderex_core::wire_adapter::engine_event_to_wire;
use coderex_core::worktree::{WorktreeConfig, WorktreeManager};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, mpsc, watch};

use crate::types::AgentType;

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Errors from agent operations.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Agent not found.
    #[error("agent not found: {0}")]
    NotFound(String),
    /// Agent is not running to receive messages.
    #[error("agent {0} is not running")]
    NotRunning(String),
    /// Failed to spawn agent.
    #[error("failed to spawn agent: {0}")]
    SpawnFailed(String),
    /// Channel closed.
    #[error("agent channel closed: {0}")]
    ChannelClosed(String),
    /// Validation failed after max retries.
    #[error("agent output validation failed: {0}")]
    ValidationFailed(String),
    /// Agent timed out.
    #[error("agent timed out after {0}s")]
    Timeout(u64),
    /// Missing required configuration.
    #[error("configuration error: {0}")]
    Configuration(String),
}

// ─── Retry Policy ───────────────────────────────────────────────────────────

/// What to do when an agent fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureAction {
    /// Retry the agent with the same or modified prompt.
    Retry,
    /// Escalate the failure to the parent session.
    EscalateToParent,
}

/// Retry policy for agent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of retries.
    pub max_retries: u32,
    /// Action to take when retries are exhausted.
    pub on_failure: FailureAction,
    /// Optional prompt to append on retry.
    pub escalation_prompt: Option<String>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            on_failure: FailureAction::EscalateToParent,
            escalation_prompt: None,
        }
    }
}

// ─── Agent Result ───────────────────────────────────────────────────────────

/// Result of a completed agent execution.
#[derive(Debug, Clone)]
pub struct AgentResult {
    /// The agent's text output.
    pub output: String,
    /// Final status.
    pub status: AgentStatus,
    /// Token usage, if available.
    pub token_usage: Option<u64>,
}

// ─── Spawn Options ──────────────────────────────────────────────────────────

/// Options for spawning an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnOptions {
    /// Run in the background (non-blocking).
    pub run_in_background: bool,
    /// Override the model to use.
    pub model_override: Option<String>,
    /// Tool whitelist — only these tools are available.
    pub allowed_tools: Option<Vec<String>>,
    /// Isolation mode. The only supported value is `"worktree"`, which runs
    /// the agent in its own git worktree so it cannot modify the caller's
    /// checkout. Any other non-`None` value is refused at spawn time rather
    /// than silently ignored.
    pub isolation: Option<String>,
    /// Working directory for the agent.
    pub cwd: Option<PathBuf>,
    /// Environment variables to inject into the agent.
    ///
    /// Sub-agents run in the parent process, so there is no child environment
    /// to inject into. A non-empty map is refused at spawn time rather than
    /// silently dropped; see [`AgentManager::spawn_agent`].
    pub extra_env: HashMap<String, String>,
    /// Whether the agent may write files (false = read-only tools).
    pub writable: bool,
    /// Maximum tokens the agent may consume (0 = unlimited).
    pub max_tokens: Option<u32>,
    /// Maximum duration in seconds (0 = unlimited).
    pub max_duration_seconds: Option<u64>,
    /// Maximum number of turns (0 = unlimited).
    pub max_turns: Option<u32>,
    /// JSON schema for validating agent output.
    pub output_schema: Option<serde_json::Value>,
    /// Retry policy on failure.
    pub retry_policy: Option<RetryPolicy>,
    /// Sandbox configuration override (None = use default sandbox).
    pub sandbox: Option<coderex_core::sandbox::SandboxConfig>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            run_in_background: false,
            model_override: None,
            allowed_tools: None,
            isolation: None,
            cwd: None,
            extra_env: HashMap::new(),
            writable: true,
            max_tokens: None,
            max_duration_seconds: None,
            max_turns: None,
            output_schema: None,
            retry_policy: None,
            sandbox: None,
        }
    }
}

// ─── Context Fork ───────────────────────────────────────────────────────────

/// Context inherited from the parent session when forking a sub-agent.
#[derive(Debug, Clone, Serialize)]
pub struct ForkedContext {
    /// Parent session ID.
    pub parent_session_id: String,
    /// Current working directory.
    pub cwd: String,
    /// Recent message count (for context estimation).
    pub recent_message_count: usize,
    /// Active tool names available to the agent.
    pub allowed_tools: Vec<String>,
    /// Environment snapshot (opt-in keys only).
    pub env_snapshot: HashMap<String, String>,
}

impl ForkedContext {
    /// Create a forked context from the parent session.
    #[must_use]
    pub fn new(parent_session_id: String, cwd: String, message_count: usize) -> Self {
        Self {
            parent_session_id,
            cwd,
            recent_message_count: message_count,
            allowed_tools: Vec::new(),
            env_snapshot: HashMap::new(),
        }
    }

    /// Set the tool whitelist for the forked agent.
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = tools;
        self
    }

    /// Add environment variables to the snapshot.
    pub fn with_env(mut self, vars: HashMap<String, String>) -> Self {
        self.env_snapshot = vars;
        self
    }
}

// ─── Agent Message ──────────────────────────────────────────────────────────

/// Message sent to an agent via SendMessage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// The agent to send to.
    pub agent_id: String,
    /// Message content.
    pub content: String,
    /// Optional message ID for request/response correlation.
    pub message_id: Option<String>,
}

// ─── Agent Status ───────────────────────────────────────────────────────────

/// Status of an agent instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Agent is starting up.
    Starting,
    /// Agent is running and accepting messages.
    Running,
    /// Agent has completed its work.
    Completed,
    /// Agent failed.
    Failed,
    /// Agent was stopped.
    Stopped,
}

// ─── Agent Instance ─────────────────────────────────────────────────────────

/// Observable event from an agent for external subscribers.
#[derive(Debug, Clone)]
pub struct AgentObservableEvent {
    /// The agent that emitted this event.
    pub agent_id: String,
    /// The engine event.
    pub event: EngineEvent,
}

/// A running agent instance.
#[derive(Debug)]
pub struct AgentInstance {
    /// Agent type.
    pub agent_type: AgentType,
    /// Spawn options used.
    pub options: SpawnOptions,
    /// Task handle for lifecycle tracking.
    pub task_handle: TaskHandle,
    /// Shared status channel, updated when the background task finishes.
    status_tx: watch::Sender<AgentStatus>,
    /// Receivers retain the latest status, so waiters cannot miss completion.
    status_rx: watch::Receiver<AgentStatus>,
    /// Forked context from parent session.
    pub context: Option<ForkedContext>,
    /// Message channel to the agent.
    pub message_tx: Option<mpsc::UnboundedSender<AgentMessage>>,
    /// Agent's output file path.
    pub output_file: Option<PathBuf>,
    /// Agent's model override.
    pub model: Option<String>,
    /// System prompt used for this agent.
    pub prompt: String,
    /// Background task handle for the running agent.
    pub task_join_handle: Option<tokio::task::JoinHandle<()>>,
    /// Directory the agent's engine actually runs in. This is the isolation
    /// worktree when one was created, otherwise the caller's cwd (or the
    /// `cwd` override). It is the value passed to the engine as its working
    /// directory.
    pub working_directory: PathBuf,
    /// Git worktree created for this agent, when isolation was requested.
    pub worktree_path: Option<PathBuf>,
}

impl AgentInstance {
    /// Create a new agent instance.
    #[must_use]
    pub fn new(agent_type: AgentType, options: SpawnOptions, task_handle: TaskHandle) -> Self {
        let model = options.model_override.clone();
        let working_directory = options.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
        let (status_tx, status_rx) = watch::channel(AgentStatus::Starting);
        Self {
            agent_type,
            options,
            task_handle,
            status_tx,
            status_rx,
            context: None,
            message_tx: None,
            output_file: None,
            model,
            prompt: String::new(),
            task_join_handle: None,
            working_directory,
            worktree_path: None,
        }
    }

    /// Return the latest lifecycle status, including background completion.
    #[must_use]
    pub fn status(&self) -> AgentStatus {
        *self.status_rx.borrow()
    }
}

// ─── Teammate Events ────────────────────────────────────────────────────────

/// Event from a teammate (agent or sub-session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TeammateEvent {
    /// Teammate went idle (no work to do).
    Idle {
        /// Agent ID.
        agent_id: String,
        /// Idle reason.
        reason: String,
    },
    /// Teammate completed its task.
    TaskCompleted {
        /// Agent ID.
        agent_id: String,
        /// Task subject.
        subject: String,
        /// Summary of work done.
        summary: String,
    },
}

/// Callback for receiving teammate events.
pub type TeammateEventCallback = Box<dyn Fn(TeammateEvent) + Send + Sync + 'static>;

/// Factory used to build an isolated tool executor for one sub-agent.
type ToolExecutorFactory =
    dyn Fn(PathBuf, Option<SandboxConfig>) -> Box<dyn ToolExecutor + Send + Sync> + Send + Sync;

// ─── AgentManager ───────────────────────────────────────────────────────────

/// Manages the lifecycle of sub-agents.
///
/// Handles:
/// - Spawning agents with context fork and tool whitelist
/// - Tracking agent state and messaging
/// - Registering externally managed agents (no execution is resumed)
/// - Broadcasting teammate events
/// - Running forked agents and background features via AI
pub struct AgentManager {
    /// Shared task manager for lifecycle tracking.
    task_manager: Arc<Mutex<TaskManager>>,
    /// Active agent instances.
    agents: HashMap<String, AgentInstance>,
    /// Callback for teammate events.
    event_callback: Option<TeammateEventCallback>,
    /// Agent ids registered as managed outside this manager.
    registered_agents: Vec<String>,
    /// API client for forked/background AI execution.
    api_client: Option<Box<dyn ApiClient>>,
    /// Default model inherited from the parent session.
    model: ModelId,
    /// Event sender for forwarding agent events to the parent engine.
    event_tx: Option<mpsc::UnboundedSender<EngineEvent>>,
    /// Parent permission state, sampled whenever a new agent starts.
    permission_checker: Option<Arc<std::sync::Mutex<PermissionChecker>>>,
    /// Interactive channel shared with the parent frontend.
    confirmation_bus: Option<ConfirmationBus>,
    /// Factory for creating tool executors for spawned agents.
    /// The factory receives the agent working directory and optional sandbox override.
    tool_executor_factory: Option<Arc<ToolExecutorFactory>>,
    /// Broadcast channel for agent observable events.
    agent_event_broadcast: Option<broadcast::Sender<AgentObservableEvent>>,
    /// Custom agent definitions loaded from config files.
    custom_agents: HashMap<String, crate::custom_agents::CustomAgentDef>,
    /// When true, spawn_agent writes a placeholder instead of requiring an API client.
    test_mode: bool,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("agents", &self.agents.len())
            .field("registered_agents", &self.registered_agents)
            .finish_non_exhaustive()
    }
}

impl AgentManager {
    /// Create a new AgentManager with the given task manager.
    #[must_use]
    pub fn new(task_manager: Arc<Mutex<TaskManager>>) -> Self {
        let (agent_event_broadcast, _) = broadcast::channel(256);
        Self {
            task_manager,
            agents: HashMap::new(),
            event_callback: None,
            registered_agents: Vec::new(),
            api_client: None,
            model: ModelId::sonnet(),
            event_tx: None,
            permission_checker: None,
            confirmation_bus: None,
            tool_executor_factory: None,
            agent_event_broadcast: Some(agent_event_broadcast),
            custom_agents: crate::custom_agents::load_custom_agents(),
            test_mode: false,
        }
    }

    /// Create a new AgentManager with its own internal task manager.
    #[must_use]
    pub fn standalone() -> Self {
        Self::new(Arc::new(Mutex::new(TaskManager::new())))
    }

    /// Set the API client for forked/background AI execution.
    pub fn set_api_client(&mut self, client: Box<dyn ApiClient>) {
        self.api_client = Some(client);
    }

    /// Set the default model used when an agent has no explicit override.
    pub fn set_model(&mut self, model: ModelId) {
        self.model = model;
    }

    /// Set the callback for teammate events.
    pub fn set_event_callback<F>(&mut self, callback: F)
    where
        F: Fn(TeammateEvent) + Send + Sync + 'static,
    {
        self.event_callback = Some(Box::new(callback));
    }

    /// Set the event sender for forwarding agent events to the parent engine.
    pub fn set_event_sender(&mut self, tx: mpsc::UnboundedSender<EngineEvent>) {
        self.event_tx = Some(tx);
    }

    /// Inherit the parent's current permission mode for each new agent.
    pub fn set_permission_checker(&mut self, checker: Arc<std::sync::Mutex<PermissionChecker>>) {
        self.permission_checker = Some(checker);
    }

    /// Route sub-agent approval prompts through the parent's frontend.
    pub fn set_confirmation_bus(&mut self, bus: ConfirmationBus) {
        self.confirmation_bus = Some(bus);
    }

    /// Set the tool executor factory for spawned agents.
    ///
    /// Since `coderex-agents` cannot depend on `coderex-tools`, the caller
    /// (e.g. the CLI or REPL) must provide a factory that creates a concrete
    /// `ToolExecutor` implementation such as `RegistryToolDispatcher`.
    pub fn set_tool_executor_factory<F>(&mut self, factory: F)
    where
        F: Fn(PathBuf, Option<SandboxConfig>) -> Box<dyn ToolExecutor + Send + Sync>
            + Send
            + Sync
            + 'static,
    {
        self.tool_executor_factory = Some(Arc::new(factory));
    }

    /// Enable test mode — spawn_agent will write a placeholder instead of
    /// requiring a real API client. Used by integration tests.
    pub fn enable_test_mode(&mut self) {
        self.test_mode = true;
    }

    /// Subscribe to agent observable events.
    #[must_use]
    pub fn subscribe_to_agent(
        &self,
        _agent_id: &str,
    ) -> Option<broadcast::Receiver<AgentObservableEvent>> {
        self.agent_event_broadcast.as_ref().map(|tx| tx.subscribe())
    }

    /// Wait for an agent to complete (synchronous execution).
    pub async fn wait_for_completion(&self, agent_id: &str) -> Result<AgentResult, AgentError> {
        let (mut status_rx, output_file) = {
            let agent = self
                .agents
                .get(agent_id)
                .ok_or_else(|| AgentError::NotFound(agent_id.to_string()))?;
            (agent.status_rx.clone(), agent.output_file.clone())
        };

        loop {
            let status = *status_rx.borrow();
            if matches!(
                status,
                AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Stopped
            ) {
                let output = if let Some(ref path) = output_file {
                    tokio::fs::read_to_string(path).await.unwrap_or_default()
                } else {
                    String::new()
                };
                let token_usage = Self::read_token_usage(output_file.as_ref()).await;
                return Ok(AgentResult {
                    output,
                    status,
                    token_usage,
                });
            }

            // `watch` retains the terminal value. Even if the agent finished
            // between the status read and this await, `changed()` returns
            // immediately instead of losing the completion notification.
            status_rx.changed().await.map_err(|_| {
                AgentError::ChannelClosed(format!("agent {agent_id} stopped without a status"))
            })?;
        }
    }

    /// Read token usage from the companion `.meta.json` file for an agent.
    async fn read_token_usage(output_file: Option<&PathBuf>) -> Option<u64> {
        let meta_path = output_file?.with_extension("meta.json");
        let content = tokio::fs::read_to_string(&meta_path).await.ok()?;
        let meta: serde_json::Value = serde_json::from_str(&content).ok()?;
        meta.get("token_usage")?.as_u64()
    }

    /// Emit a teammate event (notifies callback if registered).
    fn emit_event(&self, event: TeammateEvent) {
        if let Some(ref cb) = self.event_callback {
            cb(event);
        }
    }

    /// Emit an agent lifecycle event to the parent engine event stream.
    #[allow(dead_code)]
    fn emit_agent_lifecycle(&self, agent_id: &str, event: AgentLifecycleEvent) {
        if let Some(ref tx) = self.event_tx {
            let _ = tx.send(EngineEvent::AgentEvent {
                agent_id: agent_id.to_string(),
                event,
            });
        }
    }

    /// Spawn a new agent.
    ///
    /// Creates a task handle, forks context from the parent session,
    /// applies tool whitelist, and starts the agent process.
    ///
    /// If an API client is configured, the agent runs in a background
    /// tokio task using a real [`QueryEngine`]. If no API client is
    /// available, returns [`AgentError::Configuration`] in production.
    /// In tests, a placeholder output is written for convenience.
    ///
    /// # Arguments
    /// * `agent_type` — Type of agent to spawn.
    /// * `prompt` — User prompt / task for the agent.
    /// * `options` — Spawn configuration.
    /// * `parent_session_id` — ID of the parent session for context fork.
    /// * `cwd` — Working directory.
    /// * `message_count` — Number of recent messages for context.
    pub async fn spawn_agent(
        &mut self,
        agent_type: AgentType,
        prompt: &str,
        options: SpawnOptions,
        parent_session_id: &str,
        cwd: &str,
        message_count: usize,
    ) -> Result<&AgentInstance, AgentError> {
        // Refuse spawn options this manager cannot honor before doing any
        // work. An accepted-but-ignored isolation request is the dangerous
        // case: the caller believes the agent is confined to its own worktree
        // when it is in fact free to edit the caller's checkout.
        validate_spawn_options(&options)?;

        // Build forked context
        let context = ForkedContext::new(
            parent_session_id.to_string(),
            cwd.to_string(),
            message_count,
        )
        .with_tools(options.allowed_tools.clone().unwrap_or_default())
        .with_env(options.extra_env.clone());

        // Create task handle for tracking
        let kind = TaskKind::LocalAgent {
            agent_type: agent_type_name(&agent_type),
            pid: None,
        };

        let mut task_manager = self.task_manager.lock().await;
        let task = task_manager.create_task(kind);
        let task_id = task.id.clone();
        let _ = task_manager.mark_running(&task_id);
        let task_handle = task_manager.get_task(&task_id).unwrap().clone();
        drop(task_manager);

        // Set up message channel
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<AgentMessage>();

        // Resolve where the agent actually runs. A `worktree` isolation
        // request creates a real git worktree and runs the agent there; the
        // caller's checkout is never the agent's working directory in that
        // case. A failed worktree creation is a hard error, never a silent
        // fallback to the caller's checkout.
        let base_directory = options.cwd.clone().unwrap_or_else(|| PathBuf::from(cwd));
        let worktree_path = if options.isolation.is_some() {
            Some(create_isolation_worktree(&base_directory, &task_id)?)
        } else {
            None
        };
        let working_directory = worktree_path
            .clone()
            .unwrap_or_else(|| base_directory.clone());

        // Create output file path. Keep agent artifacts out of the project
        // unless the caller explicitly chose a working directory override.
        // When isolation created a worktree (itself a checkout) the artifacts
        // go to the temp directory so they do not pollute that checkout.
        let output_file = match (&worktree_path, &options.cwd) {
            (None, Some(dir)) => dir.join(format!("agent-{task_id}-output.txt")),
            _ => std::env::temp_dir()
                .join("coderex-agents")
                .join(format!("agent-{task_id}-output.txt")),
        };

        if let Some(parent) = output_file.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                AgentError::SpawnFailed(format!(
                    "could not create agent output directory {}: {error}",
                    parent.display()
                ))
            })?;
        }

        let agent_type_name_str = agent_type_name(&agent_type);
        let system_prompt = system_prompt_for(&agent_type, prompt, &self.custom_agents);

        // Merge custom agent options if this is a named custom agent
        let mut options = options;
        if let AgentType::Custom {
            prompt: ref custom_name,
        } = agent_type
        {
            if let Some(def) = self.custom_agents.get(custom_name) {
                if !def.allowed_tools.is_empty() && options.allowed_tools.is_none() {
                    options.allowed_tools = Some(def.allowed_tools.clone());
                }
                if options.model_override.is_none() {
                    options.model_override = def.model.clone();
                }
                if options.max_tokens.is_none() {
                    options.max_tokens = def.max_tokens;
                }
                if options.output_schema.is_none() {
                    options.output_schema = def.output_schema.clone();
                }
            }
        }

        let mut instance = AgentInstance::new(agent_type, options.clone(), task_handle);
        instance.context = Some(context);
        instance.message_tx = Some(msg_tx);
        instance.output_file = Some(output_file.clone());
        instance.working_directory = working_directory.clone();
        instance.worktree_path = worktree_path.clone();
        let _ = instance.status_tx.send(AgentStatus::Running);
        instance.prompt = system_prompt.clone();

        let agent_id = task_id.clone();
        let permission_mode = self
            .permission_checker
            .as_ref()
            .map_or_else(PermissionMode::default, |checker| {
                checker.lock().unwrap().mode()
            });

        if let Some(ref api_client) = self.api_client {
            let client = api_client.clone_box();
            let model = options
                .model_override
                .as_ref()
                .map(|m| ModelId(m.clone()))
                .unwrap_or_else(|| self.model.clone());
            let max_tokens = options.max_tokens.unwrap_or(4096);
            let max_duration = options.max_duration_seconds.unwrap_or(600);
            let max_turns = options.max_turns.unwrap_or(50);
            let allowed_tools = options.allowed_tools.clone();
            let writable = options.writable;
            let sandbox_config = options.sandbox.clone();
            let retry_policy = options.retry_policy.clone();
            let output_schema = options.output_schema.clone();
            let user_prompt = prompt.to_string();
            let output_file_path = output_file.clone();
            let agent_id_for_task = agent_id.clone();
            let event_tx = self.event_tx.clone();
            let tool_factory = self.tool_executor_factory.clone();
            let broadcast_tx = self.agent_event_broadcast.clone();
            let confirmation_bus = self.confirmation_bus.clone();
            let status_tx = instance.status_tx.clone();
            let task_manager_for_task = Arc::clone(&self.task_manager);
            let task_id_for_completion = agent_id.clone();

            let handle = tokio::spawn(async move {
                let result = run_agent_task(
                    client,
                    model,
                    system_prompt,
                    user_prompt,
                    working_directory,
                    permission_mode,
                    confirmation_bus,
                    max_tokens,
                    max_duration,
                    max_turns,
                    allowed_tools,
                    writable,
                    sandbox_config,
                    output_file_path,
                    agent_id_for_task,
                    event_tx,
                    tool_factory,
                    broadcast_tx,
                    retry_policy,
                    output_schema,
                    &mut msg_rx,
                )
                .await;

                match &result {
                    Ok(agent_result) => {
                        let _ = status_tx.send(agent_result.status);
                        let mut task_manager = task_manager_for_task.lock().await;
                        if agent_result.status == AgentStatus::Completed {
                            let _ = task_manager.mark_completed(&task_id_for_completion, Some(0));
                        } else {
                            let _ = task_manager.mark_failed(
                                &task_id_for_completion,
                                "agent execution failed".to_string(),
                            );
                        }
                    }
                    Err(error) => {
                        let _ = status_tx.send(AgentStatus::Failed);
                        let mut task_manager = task_manager_for_task.lock().await;
                        let _ =
                            task_manager.mark_failed(&task_id_for_completion, error.to_string());
                    }
                }
            });

            instance.task_join_handle = Some(handle);
        } else if cfg!(test) || self.test_mode {
            // Test convenience: write placeholder and complete immediately
            let placeholder = format!("agent completed (test mode)\n\nAgent task: {prompt}");
            let _ = tokio::fs::write(&output_file, &placeholder).await;
            let _ = instance.status_tx.send(AgentStatus::Completed);
        } else {
            return Err(AgentError::Configuration(
                "AgentManager requires an API client to spawn agents. \
                 Configure one via set_api_client() before calling spawn_agent()."
                    .into(),
            ));
        }

        self.agents.insert(agent_id.clone(), instance);

        tracing::info!(
            "agent spawned: task_id={agent_id}, type={agent_type_name_str}, prompt_len={}",
            prompt.len()
        );

        self.agents
            .get(&agent_id)
            .ok_or_else(|| AgentError::SpawnFailed("agent not found after spawn".into()))
    }

    /// Get an agent by task ID.
    #[must_use]
    pub fn get_agent(&self, id: &str) -> Option<&AgentInstance> {
        self.agents.get(id)
    }

    /// Send a message to a running agent.
    pub async fn send_message(&self, agent_id: &str, content: &str) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| AgentError::NotFound(agent_id.to_string()))?;

        if agent.status() != AgentStatus::Running {
            return Err(AgentError::NotRunning(agent_id.to_string()));
        }

        if let Some(ref tx) = agent.message_tx {
            let msg = AgentMessage {
                agent_id: agent_id.to_string(),
                content: content.to_string(),
                message_id: None,
            };
            // Best-effort send — in a real implementation the receiver
            // would be held by the agent process. For now, we ignore
            // closed-channel errors (receiver may not yet be connected).
            let _ = tx.send(msg);
            Ok(())
        } else {
            Err(AgentError::NotRunning(agent_id.to_string()))
        }
    }

    /// Mark an agent as completed and emit a TaskCompleted event.
    pub async fn complete_agent(
        &mut self,
        agent_id: &str,
        subject: &str,
        summary: &str,
    ) -> Result<(), AgentError> {
        let instance = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| AgentError::NotFound(agent_id.to_string()))?;

        let mut task_manager = self.task_manager.lock().await;
        let _ = task_manager.mark_completed(agent_id, Some(0));
        drop(task_manager);

        let _ = instance.status_tx.send(AgentStatus::Completed);

        // Emit teammate event
        self.emit_event(TeammateEvent::TaskCompleted {
            agent_id: agent_id.to_string(),
            subject: subject.to_string(),
            summary: summary.to_string(),
        });

        Ok(())
    }

    /// Mark an agent as idle and emit an Idle event.
    pub async fn mark_idle(&self, agent_id: &str, reason: &str) -> Result<(), AgentError> {
        if !self.agents.contains_key(agent_id) {
            return Err(AgentError::NotFound(agent_id.to_string()));
        }

        self.emit_event(TeammateEvent::Idle {
            agent_id: agent_id.to_string(),
            reason: reason.to_string(),
        });

        Ok(())
    }

    /// Stop an agent.
    pub async fn stop_agent(&mut self, agent_id: &str) -> Result<(), AgentError> {
        let instance = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| AgentError::NotFound(agent_id.to_string()))?;

        let mut task_manager = self.task_manager.lock().await;
        let _ = task_manager.stop_task(agent_id);
        drop(task_manager);

        let _ = instance.status_tx.send(AgentStatus::Stopped);

        Ok(())
    }

    /// List all agents.
    #[must_use]
    pub fn list_agents(&self) -> Vec<&AgentInstance> {
        self.agents.values().collect()
    }

    /// Count running agents.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.agents
            .values()
            .filter(|a| a.status() == AgentStatus::Running)
            .count()
    }

    // ─── Registered (externally managed) agents ─────────────────────────

    /// Register an agent id that is managed outside this manager.
    ///
    /// This does **not** resume execution, and it cannot: CodeRex does not
    /// persist a sub-agent's transcript, so there is no prior conversation or
    /// task to resume from. The previous name for this method
    /// (`resume_agent`) promised a resume it never performed — it created an
    /// instance marked [`AgentStatus::Running`] whose message channel had no
    /// receiver, so `send_message` reported success while nothing ever
    /// received the message.
    ///
    /// The id is recorded only so callers can ask
    /// [`AgentManager::is_registered`]. No [`AgentInstance`] is created, so
    /// `send_message`, `get_agent`, and `wait_for_completion` treat the id as
    /// unknown rather than as a running agent.
    pub fn register_agent(&mut self, agent_id: &str) -> Result<(), AgentError> {
        if self.is_registered(agent_id) {
            return Err(AgentError::SpawnFailed(format!(
                "agent {agent_id} is already registered"
            )));
        }

        self.registered_agents.push(agent_id.to_string());

        tracing::info!("agent registered (no execution attached): agent_id={agent_id}");

        Ok(())
    }

    /// Check whether an agent id was registered via [`AgentManager::register_agent`].
    #[must_use]
    pub fn is_registered(&self, agent_id: &str) -> bool {
        self.registered_agents.iter().any(|id| id == agent_id)
    }

    // ─── Forked / Background Agent Execution ─────────────────────────────────

    /// Run a forked agent via this manager's API client.
    ///
    /// Requires `set_api_client` to have been called first.
    pub async fn run_forked_agent(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        parent_context: Option<&str>,
        allowed_tools: Option<&[String]>,
        model: &coderex_core::types::ModelId,
        max_tokens: u32,
        writable: bool,
    ) -> Result<QueryResult, AgentError> {
        let client = self.api_client.as_ref().ok_or_else(|| {
            AgentError::SpawnFailed("API client not configured on AgentManager".into())
        })?;
        run_forked_agent(
            client.as_ref(),
            system_prompt,
            user_prompt,
            parent_context,
            allowed_tools,
            model,
            max_tokens,
            writable,
        )
        .await
        .map_err(|e| AgentError::SpawnFailed(e.to_string()))
    }

    /// Run a background feature via this manager's API client.
    ///
    /// Requires `set_api_client` to have been called first.
    pub async fn run_background_feature(
        &self,
        request: BackgroundFeatureRequest,
    ) -> Result<QueryResult, AgentError> {
        let client = self.api_client.as_ref().ok_or_else(|| {
            AgentError::SpawnFailed("API client not configured on AgentManager".into())
        })?;
        run_background_feature(client.as_ref(), request)
            .await
            .map_err(|e| AgentError::SpawnFailed(e.to_string()))
    }
}

/// The only `SpawnOptions::isolation` value this manager can honor.
const ISOLATION_WORKTREE: &str = "worktree";

/// Reject spawn options this manager cannot honor.
///
/// Both checks guard against the same failure mode: a request that is
/// accepted and then silently ignored. A caller that asked for worktree
/// isolation believes the agent cannot touch their checkout; a caller that
/// asked for environment variables believes the agent has them. Running the
/// agent anyway, without either, is worse than refusing to spawn it.
fn validate_spawn_options(options: &SpawnOptions) -> Result<(), AgentError> {
    if let Some(ref mode) = options.isolation {
        if !mode.eq_ignore_ascii_case(ISOLATION_WORKTREE) {
            return Err(AgentError::Configuration(format!(
                "unsupported isolation mode {mode:?}: this build can only isolate an agent in a \
                 git worktree (isolation = \"worktree\"); refusing the spawn instead of running \
                 it without the isolation the caller asked for"
            )));
        }
    }

    if !options.extra_env.is_empty() {
        let mut keys: Vec<&str> = options.extra_env.keys().map(String::as_str).collect();
        keys.sort_unstable();
        return Err(AgentError::Configuration(format!(
            "extra_env is not supported for sub-agents (requested keys: {keys:?}); sub-agents run \
             in the parent process, so there is no child environment to inject into, and setting \
             process-wide variables would leak into the parent and other agents; refusing the \
             spawn instead of ignoring the request"
        )));
    }

    Ok(())
}

/// Create a git worktree for an isolated agent and return its path.
///
/// The worktree is created from `repo` on a branch named after the agent, so
/// two concurrently isolated agents never collide on a branch name. A failure
/// here is a hard error: an agent that asked for isolation must never fall
/// back to running in the caller's checkout.
///
/// The worktree is left in place after the agent finishes — it holds the
/// agent's work, which is the point of isolating it — so callers that want it
/// removed should do so explicitly.
fn create_isolation_worktree(repo: &Path, agent_id: &str) -> Result<PathBuf, AgentError> {
    let config = WorktreeConfig {
        base_dir: repo.join(".coderex").join("worktrees"),
        branch_prefix: "coderex".to_string(),
        auto_cleanup: true,
    };
    let mut manager = WorktreeManager::new(config);
    let branch = format!("coderex-agent-{agent_id}");
    let repo_path = repo.to_path_buf();

    manager
        .create_worktree(&repo_path, Some(&branch))
        .map(|result| result.path)
        .map_err(|error| {
            AgentError::SpawnFailed(format!(
                "could not create an isolation worktree for agent {agent_id} from {}: {error}",
                repo.display()
            ))
        })
}

/// Derive a system prompt from an agent type and user prompt.
fn system_prompt_for(
    agent_type: &AgentType,
    user_prompt: &str,
    custom_agents: &HashMap<String, crate::custom_agents::CustomAgentDef>,
) -> String {
    match agent_type {
        AgentType::GeneralPurpose => format!(
            "You are a general-purpose sub-agent. You help with a wide variety of tasks \
             by reasoning step by step and using available tools when needed.\n\nTask: {user_prompt}"
        ),
        AgentType::Explore { thoroughness } => {
            let depth = match thoroughness {
                crate::types::Thoroughness::Quick => "quick scan",
                crate::types::Thoroughness::Medium => "moderate depth",
                crate::types::Thoroughness::VeryThorough => "full recursive analysis",
            };
            format!(
                "You are an exploration agent specialized in analyzing codebases. \
                 Thoroughness level: {depth}. Search thoroughly and report your findings.\n\nTask: {user_prompt}"
            )
        }
        AgentType::Plan => format!(
            "You are a planning agent. You analyze requirements and produce detailed, \
             actionable implementation plans. Break down complex tasks into clear steps.\n\nTask: {user_prompt}"
        ),
        AgentType::Verification => format!(
            "You are a verification agent. You review code, plans, or outputs for \
             correctness, completeness, and adherence to requirements. Report issues clearly.\n\nTask: {user_prompt}"
        ),
        AgentType::ClaudeCodeGuide => format!(
            "You are an expert on Claude Code. You answer questions about features, \
             usage, configuration, and best practices for the Claude Code CLI tool.\n\nTask: {user_prompt}"
        ),
        AgentType::StatuslineSetup => format!(
            "You help configure the Claude Code status line. You suggest settings, \
             formatting, and customizations for the statusline display.\n\nTask: {user_prompt}"
        ),
        AgentType::Custom { prompt } => {
            // If prompt matches a custom agent name, use its system prompt
            if let Some(def) = custom_agents.get(prompt) {
                return format!("{}\n\nTask: {user_prompt}", def.system_prompt);
            }
            if prompt.is_empty() {
                user_prompt.to_string()
            } else {
                format!("{prompt}\n\nTask: {user_prompt}")
            }
        }
    }
}

/// Run the actual agent task inside a spawned tokio task.
///
/// Creates a [`QueryEngine`], runs the submission, collects text output,
/// and forwards lifecycle events to the parent.
#[allow(clippy::too_many_arguments)]
async fn run_agent_task(
    api_client: Box<dyn ApiClient>,
    model: ModelId,
    system_prompt: String,
    user_prompt: String,
    working_directory: PathBuf,
    permission_mode: PermissionMode,
    confirmation_bus: Option<ConfirmationBus>,
    max_tokens: u32,
    max_duration: u64,
    max_turns: u32,
    allowed_tools: Option<Vec<String>>,
    writable: bool,
    sandbox_config: Option<SandboxConfig>,
    output_file: PathBuf,
    agent_id: String,
    parent_event_tx: Option<mpsc::UnboundedSender<EngineEvent>>,
    tool_factory: Option<Arc<ToolExecutorFactory>>,
    broadcast_tx: Option<broadcast::Sender<AgentObservableEvent>>,
    retry_policy: Option<RetryPolicy>,
    output_schema: Option<serde_json::Value>,
    msg_rx: &mut mpsc::UnboundedReceiver<AgentMessage>,
) -> Result<AgentResult, AgentError> {
    // Notify parent that the agent started
    if let Some(ref tx) = parent_event_tx {
        let _ = tx.send(EngineEvent::AgentEvent {
            agent_id: agent_id.clone(),
            event: AgentLifecycleEvent::Started,
        });
    }

    let max_retries = retry_policy.as_ref().map(|r| r.max_retries).unwrap_or(0);
    let _on_failure = retry_policy
        .as_ref()
        .map(|r| r.on_failure)
        .unwrap_or(FailureAction::EscalateToParent);
    let escalation_prompt = retry_policy
        .as_ref()
        .and_then(|r| r.escalation_prompt.clone());

    // Set up agent log file
    let log_dir = home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coderex")
        .join("agent_logs");
    let log_path = log_dir.join(format!("{agent_id}.jsonl"));

    let mut attempt_prompt = user_prompt;
    let mut final_output = String::new();
    let mut final_status = AgentStatus::Completed;
    let mut final_error: Option<String> = None;
    let mut total_token_usage: Option<u64> = None;

    for attempt in 0..=max_retries {
        // Drain any messages sent by the parent before this attempt
        while let Ok(msg) = msg_rx.try_recv() {
            attempt_prompt.push_str(&format!("\n\n[Message from parent]: {}", msg.content));
        }

        let session = SessionState::new(working_directory.clone());
        let checker = PermissionChecker::new(permission_mode);
        let permission_engine = PermissionEngine::new(checker);

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EngineEvent>();

        let mut engine = QueryEngine::new(
            api_client.clone_box(),
            model.clone(),
            session,
            permission_engine,
            event_tx,
        );

        // A sub-agent must never read from the parent's terminal directly.
        // Interactive questions use the parent's structured channels instead.
        engine.set_elicitation_handler(Box::new(SdkElicitationHandler::default()));
        if let Some(bus) = confirmation_bus.clone() {
            engine.set_confirmation_bus(bus);
        }

        if let Some(ref factory) = tool_factory {
            engine.set_tool_executor(factory(working_directory.clone(), sandbox_config.clone()));
        }

        let mut options = SubmitOptions::default();
        options.custom_system_prompt = Some(system_prompt.clone());
        options.allowed_tools_override = if writable {
            allowed_tools.clone()
        } else {
            let read_only = vec!["Read".into(), "Grep".into(), "Glob".into()];
            match allowed_tools.clone() {
                Some(tools) => Some(
                    tools
                        .into_iter()
                        .filter(|t| read_only.iter().any(|r: &String| r.eq_ignore_ascii_case(t)))
                        .collect(),
                ),
                None => Some(read_only),
            }
        };
        options.max_turns = max_turns;
        options.task_budget_tokens = u64::from(max_tokens);

        let timeout = Duration::from_secs(max_duration);
        let engine_result =
            tokio::time::timeout(timeout, engine.submit(attempt_prompt.clone(), options)).await;

        // Capture total token usage before dropping the engine
        let usage = engine.total_usage();
        let attempt_tokens = usage.input_tokens + usage.output_tokens;
        total_token_usage = Some(total_token_usage.unwrap_or(0) + attempt_tokens as u64);

        // Drop engine to release event_tx so recv drains reliably
        drop(engine);

        // Collect text output from events and forward to observability channels
        let mut attempt_output = String::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await
        {
            // Also drain any messages from parent while collecting events
            while let Ok(msg) = msg_rx.try_recv() {
                attempt_prompt.push_str(&format!("\n\n[Message from parent]: {}", msg.content));
            }

            if let EngineEvent::TextDelta(ref text) = event {
                attempt_output.push_str(text);
            }

            // Forward to broadcast subscribers
            if let Some(ref tx) = broadcast_tx {
                let observable = AgentObservableEvent {
                    agent_id: agent_id.clone(),
                    event: event.clone(),
                };
                let _ = tx.send(observable);
            }

            // Append to NDJSON log file
            if let Some(wire_event) = engine_event_to_wire(&event) {
                if let Ok(json) = serde_json::to_string(&wire_event) {
                    let _ = async {
                        let _ = tokio::fs::create_dir_all(&log_dir).await;
                        let mut file = tokio::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&log_path)
                            .await?;
                        use tokio::io::AsyncWriteExt;
                        file.write_all(json.as_bytes()).await?;
                        file.write_all(b"\n").await?;
                        Ok::<(), std::io::Error>(())
                    }
                    .await;
                }
            }
        }

        match engine_result {
            Ok(Ok(_)) => {
                // Validate output against schema if provided
                if let Some(ref schema) = output_schema {
                    let validator =
                        StructuredOutputValidator::new(schema.clone()).with_max_retries(2);
                    match validator.validate(&attempt_output, 0) {
                        ValidationResult::Valid(_) => {}
                        ValidationResult::Invalid { error, .. } => {
                            if attempt < max_retries {
                                let retry_note =
                                    format!("\n\n{}", validator.retry_instruction(&error));
                                attempt_prompt.push_str(&retry_note);
                                continue;
                            }
                            final_output = attempt_output;
                            final_status = AgentStatus::Failed;
                            final_error = Some(format!("validation failed: {error}"));
                            break;
                        }
                    }
                }
                final_output = attempt_output;
                final_status = AgentStatus::Completed;
                final_error = None;
                break;
            }
            Ok(Err(e)) => {
                let err_msg = format!("engine error: {e}");
                if attempt < max_retries {
                    // Build retry prompt with error context
                    let mut retry_note =
                        format!("\n\n[Previous attempt failed: {err_msg}. Retrying...]");
                    if let Some(ref ep) = escalation_prompt {
                        retry_note.push_str(&format!("\n{ep}"));
                    }
                    attempt_prompt.push_str(&retry_note);
                    continue;
                }
                final_output = attempt_output;
                final_status = AgentStatus::Failed;
                final_error = Some(err_msg);
            }
            Err(_) => {
                let err_msg = format!("agent timed out after {max_duration}s");
                if attempt < max_retries {
                    let mut retry_note = format!(
                        "\n\n[Previous attempt timed out after {max_duration}s. Retrying...]"
                    );
                    if let Some(ref ep) = escalation_prompt {
                        retry_note.push_str(&format!("\n{ep}"));
                    }
                    attempt_prompt.push_str(&retry_note);
                    continue;
                }
                final_output = attempt_output;
                final_status = AgentStatus::Failed;
                final_error = Some(err_msg);
            }
        }
    }

    let (status, error) = (final_status, final_error);

    // Notify parent of completion/failure
    if let Some(ref tx) = parent_event_tx {
        let lifecycle = if let Some(ref err) = error {
            AgentLifecycleEvent::Failed { error: err.clone() }
        } else {
            AgentLifecycleEvent::Completed {
                summary: final_output.clone(),
            }
        };
        let _ = tx.send(EngineEvent::AgentEvent {
            agent_id: agent_id.clone(),
            event: lifecycle,
        });
    }

    // Write output to file
    let _ = tokio::fs::write(&output_file, &final_output).await;

    // Write companion meta file with token usage and final status
    let meta_path = output_file.with_extension("meta.json");
    let meta = serde_json::json!({
        "status": status,
        "token_usage": total_token_usage,
    });
    let _ = tokio::fs::write(&meta_path, meta.to_string()).await;

    Ok(AgentResult {
        output: final_output,
        status,
        token_usage: total_token_usage,
    })
}

/// Get the string name of an agent type.
fn agent_type_name(t: &AgentType) -> String {
    match t {
        AgentType::GeneralPurpose => "general_purpose".into(),
        AgentType::Explore { .. } => "explore".into(),
        AgentType::Plan => "plan".into(),
        AgentType::Verification => "verification".into(),
        AgentType::ClaudeCodeGuide => "claude_code_guide".into(),
        AgentType::StatuslineSetup => "statusline_setup".into(),
        AgentType::Custom { .. } => "custom".into(),
    }
}

// ─── AgentManager as command backend (10.5) ─────────────────────────────────

/// Execute a command via a sub-agent instead of shell.
///
/// Spawns a short-lived agent with the command as its prompt,
/// waits for completion, and returns the result.
pub async fn execute_via_agent(
    manager: &mut AgentManager,
    command: &str,
    parent_session_id: &str,
    cwd: &str,
) -> Result<String, AgentError> {
    let options = SpawnOptions {
        run_in_background: false,
        cwd: Some(PathBuf::from(cwd)),
        ..Default::default()
    };

    let agent_type = AgentType::Custom {
        prompt: command.to_string(),
    };

    let instance = manager
        .spawn_agent(agent_type, command, options, parent_session_id, cwd, 0)
        .await?;

    let task_id = instance.task_handle.id.clone();
    let output_file = instance.output_file.clone();

    // Wait for the agent task to complete
    manager.wait_for_completion(&task_id).await?;

    // Read output file if it exists
    if let Some(ref path) = output_file {
        if path.exists() {
            return tokio::fs::read_to_string(path)
                .await
                .map_err(|_| AgentError::SpawnFailed("could not read output".into()));
        }
    }

    Ok("agent completed (no output file)".into())
}

// ─── Dream agent spawning (19.2) ────────────────────────────────────────────

/// Spawn a read-only dream agent to analyze sessions and produce a summary.
///
/// Forks a context-limited agent with the dream prompt, restricts it to
/// read-only tools, and waits for the analysis output.
///
/// # Arguments
/// * `manager` — The AgentManager to spawn the dream agent on.
/// * `dream_prompt` — Instructions for the dream agent (session summaries + analysis tasks).
/// * `parent_session_id` — Parent session for context fork.
/// * `cwd` — Working directory / project root.
/// * `session_summaries` — Recent session summaries to include in the prompt.
///
/// # Returns
/// The dream agent's analysis output as a string.
pub async fn spawn_dream_agent(
    manager: &mut AgentManager,
    dream_prompt: &str,
    parent_session_id: &str,
    cwd: &str,
    session_summaries: &[&str],
) -> Result<String, AgentError> {
    // Read-only tool whitelist for dream agents
    let allowed_tools = vec!["Read".into(), "Glob".into(), "Grep".into()];

    let options = SpawnOptions {
        run_in_background: true,
        allowed_tools: Some(allowed_tools),
        cwd: Some(PathBuf::from(cwd)),
        ..Default::default()
    };

    let agent_type = AgentType::Custom {
        prompt: format!(
            "Dream agent: Analyze recent sessions and produce a structured summary.\n\n{}\n\nSession summaries:\n{}",
            dream_prompt,
            session_summaries
                .iter()
                .enumerate()
                .map(|(i, s)| format!("## Session {}\n{}", i + 1, s))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };

    let instance = manager
        .spawn_agent(
            agent_type,
            dream_prompt,
            options,
            parent_session_id,
            cwd,
            session_summaries.len(),
        )
        .await?;

    let output_file = instance.output_file.clone();

    // Read output file if it exists (for no-API-client fallback it is written
    // immediately; in production the background task writes it later).
    if let Some(ref path) = output_file {
        if path.exists() {
            return tokio::fs::read_to_string(path)
                .await
                .map_err(|_| AgentError::SpawnFailed("could not read dream output".into()));
        }
    }

    Ok("dream agent spawned (background task running)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> AgentManager {
        AgentManager::standalone()
    }

    #[tokio::test]
    async fn manager_starts_empty() {
        let mgr = make_manager();
        assert!(mgr.list_agents().is_empty());
        assert_eq!(mgr.running_count(), 0);
    }

    #[tokio::test]
    async fn spawn_agent_creates_instance() {
        let mut mgr = make_manager();
        let agent_type = AgentType::GeneralPurpose;
        let options = SpawnOptions {
            run_in_background: true,
            allowed_tools: Some(vec!["Read".into(), "Bash".into()]),
            ..Default::default()
        };

        let instance = mgr
            .spawn_agent(
                agent_type,
                "do something",
                options,
                "sess_1",
                "/tmp/test",
                5,
            )
            .await
            .unwrap();

        // Without an API client the agent completes immediately.
        assert_eq!(instance.status(), AgentStatus::Completed);
        assert!(instance.context.is_some());
        let ctx = instance.context.as_ref().unwrap();
        assert_eq!(ctx.parent_session_id, "sess_1");
        assert_eq!(ctx.allowed_tools, vec!["Read", "Bash"]);
    }

    #[tokio::test]
    async fn spawn_multiple_agents() {
        let mut mgr = make_manager();

        for i in 0..3 {
            mgr.spawn_agent(
                AgentType::GeneralPurpose,
                &format!("task {i}"),
                SpawnOptions::default(),
                "sess_1",
                "/tmp",
                0,
            )
            .await
            .unwrap();
        }

        // Without an API client agents complete immediately.
        assert_eq!(mgr.running_count(), 0);
        assert_eq!(mgr.list_agents().len(), 3);
    }

    #[tokio::test]
    async fn send_message_to_completed_agent_errors() {
        let mut mgr = make_manager();
        mgr.spawn_agent(
            AgentType::GeneralPurpose,
            "hello",
            SpawnOptions::default(),
            "sess_1",
            "/tmp",
            0,
        )
        .await
        .unwrap();

        let agent_id = mgr.list_agents()[0].task_handle.id.clone();

        // Without an API client the agent completes immediately,
        // so sending a message fails with NotRunning.
        let result = mgr.send_message(&agent_id, "follow up").await;
        assert!(matches!(result, Err(AgentError::NotRunning(_))));
    }

    #[tokio::test]
    async fn send_message_to_nonexistent_agent_errors() {
        let mgr = make_manager();
        let result = mgr.send_message("nope", "hi").await;
        assert!(matches!(result, Err(AgentError::NotFound(_))));
    }

    #[tokio::test]
    async fn complete_agent_emits_event() {
        let mut mgr = make_manager();
        mgr.spawn_agent(
            AgentType::GeneralPurpose,
            "test",
            SpawnOptions::default(),
            "sess_1",
            "/tmp",
            0,
        )
        .await
        .unwrap();

        let agent_id = mgr.list_agents()[0].task_handle.id.clone();

        // Set up event callback
        let event_received = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = event_received.clone();
        mgr.set_event_callback(move |event| {
            if let TeammateEvent::TaskCompleted { .. } = event {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        mgr.complete_agent(&agent_id, "test task", "done")
            .await
            .unwrap();

        assert!(event_received.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(mgr.list_agents()[0].status(), AgentStatus::Completed);
    }

    #[tokio::test]
    async fn mark_idle_emits_event() {
        let mut mgr = make_manager();
        mgr.spawn_agent(
            AgentType::GeneralPurpose,
            "test",
            SpawnOptions::default(),
            "sess_1",
            "/tmp",
            0,
        )
        .await
        .unwrap();

        let agent_id = mgr.list_agents()[0].task_handle.id.clone();

        let event_received = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = event_received.clone();
        mgr.set_event_callback(move |event| {
            if let TeammateEvent::Idle { .. } = event {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        mgr.mark_idle(&agent_id, "no more work").await.unwrap();

        assert!(event_received.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stop_agent() {
        let mut mgr = make_manager();
        mgr.spawn_agent(
            AgentType::GeneralPurpose,
            "test",
            SpawnOptions::default(),
            "sess_1",
            "/tmp",
            0,
        )
        .await
        .unwrap();

        let agent_id = mgr.list_agents()[0].task_handle.id.clone();
        mgr.stop_agent(&agent_id).await.unwrap();

        assert_eq!(mgr.list_agents()[0].status(), AgentStatus::Stopped);
    }

    #[test]
    fn register_agent_tracks_without_running() {
        let mut mgr = make_manager();
        mgr.register_agent("external_1").unwrap();

        assert!(mgr.is_registered("external_1"));
        // Registration starts nothing: there is no task and no instance, so
        // the agent is not counted as running and cannot be fetched.
        assert_eq!(mgr.running_count(), 0);
        assert!(mgr.get_agent("external_1").is_none());
    }

    #[test]
    fn register_duplicate_agent_errors() {
        let mut mgr = make_manager();
        mgr.register_agent("dup_1").unwrap();

        let result = mgr.register_agent("dup_1");
        assert!(matches!(result, Err(AgentError::SpawnFailed(_))));
    }

    #[tokio::test]
    async fn send_message_to_registered_agent_errors() {
        let mut mgr = make_manager();
        mgr.register_agent("external_1").unwrap();

        // A registered agent has no receiver; the message must be refused
        // rather than silently dropped.
        let result = mgr.send_message("external_1", "hello").await;
        assert!(matches!(result, Err(AgentError::NotFound(_))));
    }

    /// Create a git repository with one commit so `git worktree add` can run.
    fn init_git_repo(dir: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "init"]);
    }

    #[tokio::test]
    async fn isolation_worktree_runs_agent_in_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());

        let mut mgr = make_manager();
        let instance = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "do work",
                SpawnOptions {
                    isolation: Some("worktree".into()),
                    ..Default::default()
                },
                "sess_iso",
                repo.path().to_str().unwrap(),
                0,
            )
            .await
            .unwrap();

        let worktree = instance
            .worktree_path
            .clone()
            .expect("isolation must create a worktree");
        assert!(worktree.is_dir(), "worktree {} missing", worktree.display());
        assert_ne!(worktree.as_path(), repo.path());
        // The engine's working directory is the worktree, not the checkout.
        assert_eq!(instance.working_directory, worktree);
        // The worktree is a real checkout of the repo.
        assert!(worktree.join("README.md").is_file());
        // The agent's artifacts do not land in the caller's checkout.
        let output = instance.output_file.clone().unwrap();
        assert!(!output.starts_with(repo.path()));
    }

    #[tokio::test]
    async fn isolation_unknown_mode_is_refused() {
        let mut mgr = make_manager();
        let result = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "do work",
                SpawnOptions {
                    isolation: Some("container".into()),
                    ..Default::default()
                },
                "sess_iso",
                "/tmp",
                0,
            )
            .await;

        assert!(matches!(result, Err(AgentError::Configuration(_))));
        assert!(mgr.list_agents().is_empty());
    }

    #[tokio::test]
    async fn isolation_worktree_without_git_repo_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut mgr = make_manager();
        let result = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "do work",
                SpawnOptions {
                    isolation: Some("worktree".into()),
                    ..Default::default()
                },
                "sess_iso",
                dir.path().to_str().unwrap(),
                0,
            )
            .await;

        // No silent fallback to the caller's directory when isolation fails.
        assert!(matches!(result, Err(AgentError::SpawnFailed(_))));
        assert!(mgr.list_agents().is_empty());
    }

    #[tokio::test]
    async fn extra_env_is_refused() {
        let mut mgr = make_manager();
        let mut env = HashMap::new();
        env.insert("SECRET".to_string(), "value".to_string());

        let result = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "do work",
                SpawnOptions {
                    extra_env: env,
                    ..Default::default()
                },
                "sess_env",
                "/tmp",
                0,
            )
            .await;

        assert!(matches!(result, Err(AgentError::Configuration(_))));
        assert!(mgr.list_agents().is_empty());
    }

    #[tokio::test]
    async fn execute_via_agent_runs_command() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = make_manager();
        let output = super::execute_via_agent(
            &mut mgr,
            "explain this code",
            "sess_1",
            tmp.path().to_str().unwrap(),
        )
        .await
        .unwrap();
        assert!(output.contains("agent completed"));
    }

    #[test]
    fn agent_type_name_maps() {
        assert_eq!(
            agent_type_name(&AgentType::GeneralPurpose),
            "general_purpose"
        );
        assert_eq!(agent_type_name(&AgentType::Plan), "plan");
        assert_eq!(agent_type_name(&AgentType::Verification), "verification");
        assert_eq!(
            agent_type_name(&AgentType::ClaudeCodeGuide),
            "claude_code_guide"
        );
        assert_eq!(
            agent_type_name(&AgentType::StatuslineSetup),
            "statusline_setup"
        );
        assert_eq!(
            agent_type_name(&AgentType::Explore {
                thoroughness: crate::types::Thoroughness::Medium
            }),
            "explore"
        );
        assert_eq!(
            agent_type_name(&AgentType::Custom {
                prompt: "test".into()
            }),
            "custom"
        );
    }

    #[test]
    fn spawn_options_defaults() {
        let opts = SpawnOptions::default();
        assert!(!opts.run_in_background);
        assert!(opts.model_override.is_none());
        assert!(opts.allowed_tools.is_none());
        assert!(opts.isolation.is_none());
        assert!(opts.cwd.is_none());
        assert!(opts.extra_env.is_empty());
        assert!(opts.writable);
    }

    #[test]
    fn forked_context_builder() {
        let ctx = ForkedContext::new("parent".into(), "/src".into(), 42)
            .with_tools(vec!["Read".into(), "Bash".into()]);

        assert_eq!(ctx.parent_session_id, "parent");
        assert_eq!(ctx.cwd, "/src");
        assert_eq!(ctx.recent_message_count, 42);
        assert_eq!(ctx.allowed_tools, vec!["Read", "Bash"]);
    }

    #[test]
    fn agent_message_serialization() {
        let msg = AgentMessage {
            agent_id: "agent_1".into(),
            content: "hello".into(),
            message_id: Some("msg_1".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("agent_1"));
        assert!(json.contains("hello"));
    }

    #[tokio::test]
    async fn spawn_dream_agent_runs_with_readonly_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = make_manager();
        let dream_prompt = "Analyze recent sessions for patterns.";
        let summaries = vec!["Session 1: Fixed auth bug", "Session 2: Added tests"];

        let output = super::spawn_dream_agent(
            &mut mgr,
            dream_prompt,
            "sess_parent",
            tmp.path().to_str().unwrap(),
            &summaries,
        )
        .await
        .unwrap();

        assert!(output.contains("completed"));
        // Verify the agent was spawned with read-only tool whitelist
        let agents = mgr.list_agents();
        if let Some(agent) = agents.first() {
            if let Some(ctx) = &agent.context {
                assert_eq!(ctx.allowed_tools, vec!["Read", "Glob", "Grep"]);
            }
        }
    }
}
