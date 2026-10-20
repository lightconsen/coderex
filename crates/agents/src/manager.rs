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
use coderex_core::job_record::{JobKind, JobRecord, JobState, JobStore};
use coderex_core::orchestrator::{BackgroundFeatureRequest, QueryResult, run_background_feature};
use coderex_core::permission_engine::{BackgroundAgentDenyHook, PermissionEngine};
use coderex_core::permissions::{PermissionChecker, PermissionMode};
use coderex_core::sandbox::SandboxConfig;
use coderex_core::state::SessionState;
use coderex_core::task::{TaskHandle, TaskKind, TaskManager, now_ms};
use coderex_core::tool_executor::ToolExecutor;
use coderex_core::types::{
    AgentLifecycleEvent, EngineEvent, Message, MessageContent, ModelId, Role, ToolCall, ToolResult,
};
use coderex_core::wire_adapter::engine_event_to_wire;
use coderex_core::wire_protocol::{WireEvent, decode_event};
use coderex_core::worktree::{WorktreeConfig, WorktreeManager};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, mpsc, watch};

use crate::custom_agents::CustomAgentDef;
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
    /// A named custom agent was requested but no definition matches.
    #[error("unknown custom agent {name:?}; available agents: {available}")]
    UnknownCustomAgent {
        /// The name the caller asked for.
        name: String,
        /// Comma-separated names that are configured, or an explanation when
        /// none are.
        available: String,
    },
}

/// Describe the available custom agent names for an error message.
fn available_agents_description(names: &[String]) -> String {
    if names.is_empty() {
        "none configured (add JSON definitions under ~/.coderex/agents/)".to_string()
    } else {
        names.join(", ")
    }
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
///
/// Missing fields in a JSON policy fall back to [`RetryPolicy::default`], so
/// a caller can send just `{"max_retries": 3}` (the Agent tool's schema does
/// not require the other keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    /// Parent conversation messages the fork should reuse verbatim so the
    /// provider's prefix cache is hit instead of a fresh prefix being paid
    /// for. Empty means "start a fresh prefix".
    #[serde(default)]
    pub parent_prefix: Vec<serde_json::Value>,
    /// Parent request params that must match for `parent_prefix` to be
    /// cache-safe. See [`CacheSafeParams`].
    #[serde(default)]
    pub parent_cache_safe_params: Option<CacheSafeParams>,
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
            parent_prefix: Vec::new(),
            parent_cache_safe_params: None,
        }
    }
}

/// A snapshot of the parent request, taken at the point a sub-agent is
/// spawned.
///
/// The tool layer cannot reach the parent engine directly, so the session
/// supplies a provider that returns the parent conversation and the request
/// components that determine prompt-cache reuse. [`crate::SpawnOptions`]
/// turns this into `parent_prefix` + `parent_cache_safe_params`; the manager
/// then stores it in [`ForkedContext`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentContextSnapshot {
    /// Parent conversation messages, serialized exactly as they were sent.
    pub messages: Vec<serde_json::Value>,
    /// Model the parent request used.
    pub model: String,
    /// System prompt the parent request sent.
    pub system_prompt: String,
    /// Tool names the parent advertised, in order.
    pub tool_names: Vec<String>,
}

impl ParentContextSnapshot {
    /// Create a snapshot from the parent request's components.
    #[must_use]
    pub fn new(
        messages: Vec<serde_json::Value>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        tool_names: Vec<String>,
    ) -> Self {
        Self {
            messages,
            model: model.into(),
            system_prompt: system_prompt.into(),
            tool_names,
        }
    }

    /// The cache-safety parameters implied by this snapshot.
    #[must_use]
    pub fn cache_safe_params(&self) -> CacheSafeParams {
        CacheSafeParams::new(
            self.model.clone(),
            self.system_prompt.clone(),
            self.tool_names.clone(),
        )
    }

    /// Whether there is a conversation prefix worth reusing.
    #[must_use]
    pub fn has_prefix(&self) -> bool {
        !self.messages.is_empty()
    }
}

// ─── Context Fork ───────────────────────────────────────────────────────────

/// Request parameters that must be identical between the parent request and a
/// forked request for the provider's prompt-prefix cache to be reusable.
///
/// Providers cache on an exact byte prefix: the cacheable region starts at the
/// system prompt and covers the leading messages, and it is only hit when the
/// system prompt, the tool set and the model match what the parent sent. A
/// fork that reuses the parent's conversation prefix but changes any of these
/// pays full price for the prefix it thought it was reusing. Capturing them
/// together lets a caller check cache safety before spending the tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSafeParams {
    /// Model id the parent request used.
    pub model: String,
    /// System prompt the parent request sent, verbatim.
    pub system_prompt: String,
    /// Tool names the parent request advertised, in order.
    pub tool_names: Vec<String>,
}

impl CacheSafeParams {
    /// Create cache-safe params from the parent request's components.
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        tool_names: Vec<String>,
    ) -> Self {
        Self {
            model: model.into(),
            system_prompt: system_prompt.into(),
            tool_names,
        }
    }
}

/// Context inherited from the parent session when forking a sub-agent.
///
/// The fork is *cache-aware*: it carries the parent's conversation prefix
/// verbatim so the forked request reuses the provider's cached prefix instead
/// of starting a fresh one. `fork_point` records where the fork diverges —
/// everything up to and including that index is the parent's, and the
/// sub-agent's own task is appended after it.
///
/// CodeRex messages carry no stable uuid, so the fork point is an index into
/// the prefix rather than an id; `prefix_fingerprint` gives the same
/// drift-detection a uuid would, by hashing the exact bytes of the prefix.
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
    /// Parent conversation messages reused verbatim by the fork.
    pub conversation_prefix: Vec<serde_json::Value>,
    /// Index one past the last parent message included in the prefix — the
    /// point at which the fork's own messages begin.
    pub fork_point: usize,
    /// Params that must match the parent for the prefix to be cache-safe.
    pub cache_safe_params: Option<CacheSafeParams>,
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
            conversation_prefix: Vec::new(),
            fork_point: 0,
            cache_safe_params: None,
        }
    }

    /// Set the tool whitelist for the forked agent.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = tools;
        self
    }

    /// Add environment variables to the snapshot.
    #[must_use]
    pub fn with_env(mut self, vars: HashMap<String, String>) -> Self {
        self.env_snapshot = vars;
        self
    }

    /// Reuse the parent's conversation prefix, forking after the last message.
    #[must_use]
    pub fn with_conversation_prefix(mut self, prefix: Vec<serde_json::Value>) -> Self {
        self.fork_point = prefix.len();
        self.conversation_prefix = prefix;
        self
    }

    /// Record the params that must match the parent for a cache hit.
    #[must_use]
    pub fn with_cache_safe_params(mut self, params: CacheSafeParams) -> Self {
        self.cache_safe_params = Some(params);
        self
    }

    /// Whether this fork actually reuses a parent prefix.
    #[must_use]
    pub fn reuses_parent_prefix(&self) -> bool {
        !self.conversation_prefix.is_empty()
    }

    /// Build the message list for a forked request: the parent's prefix,
    /// byte-for-byte, followed by the sub-agent's task.
    ///
    /// The prefix is cloned rather than re-derived, so it serializes to the
    /// same bytes the parent sent and the provider's prefix cache applies.
    #[must_use]
    pub fn fork_messages(&self, task: &str) -> Vec<serde_json::Value> {
        let mut messages = self.conversation_prefix.clone();
        messages.push(serde_json::json!({
            "role": "user",
            "content": task,
        }));
        messages
    }

    /// A stable fingerprint of the exact prefix bytes.
    ///
    /// Two forks with the same fingerprint will present the same cacheable
    /// prefix; a change in any message changes it. This is the CodeRex
    /// stand-in for a fork-point uuid.
    #[must_use]
    pub fn prefix_fingerprint(&self) -> String {
        // FNV-1a over the serialized prefix. A stable, dependency-free hash is
        // enough: this is drift detection, not a security boundary.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        if let Ok(bytes) = serde_json::to_vec(&self.conversation_prefix) {
            for byte in bytes {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        format!("{hash:016x}")
    }

    /// Whether a request with these components would actually hit the
    /// parent's prefix cache.
    ///
    /// Returns `false` when no cache-safe params were recorded, or when the
    /// model, system prompt or tool set differ from the parent's.
    #[must_use]
    pub fn is_cache_safe(&self, model: &str, system_prompt: &str, tool_names: &[String]) -> bool {
        self.cache_safe_params.as_ref().is_some_and(|params| {
            params.model == model
                && params.system_prompt == system_prompt
                && params.tool_names == tool_names
        })
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

/// A filtered subscription to [`AgentObservableEvent`]s.
///
/// The manager broadcasts every agent's events on one channel; this wrapper
/// drops events belonging to other agents so a subscriber sees only the agent
/// it asked for (or all of them, for [`AgentManager::subscribe_all`]).
#[derive(Debug)]
pub struct AgentSubscription {
    agent_id: Option<String>,
    rx: broadcast::Receiver<AgentObservableEvent>,
}

impl AgentSubscription {
    /// The agent this subscription is filtered to, or `None` for all agents.
    #[must_use]
    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    /// Receive the next event for this subscription's agent.
    ///
    /// Events for other agents are skipped. Lags are reported by the
    /// underlying broadcast channel; the next call resumes after the gap.
    pub async fn recv(&mut self) -> Result<AgentObservableEvent, broadcast::error::RecvError> {
        loop {
            let event = self.rx.recv().await?;
            if Self::matches(self.agent_id.as_deref(), &event) {
                return Ok(event);
            }
        }
    }

    /// Try to receive the next event for this subscription's agent without
    /// waiting.
    pub fn try_recv(&mut self) -> Result<AgentObservableEvent, broadcast::error::TryRecvError> {
        loop {
            let event = self.rx.try_recv()?;
            if Self::matches(self.agent_id.as_deref(), &event) {
                return Ok(event);
            }
        }
    }

    fn matches(agent_id: Option<&str>, event: &AgentObservableEvent) -> bool {
        agent_id.is_none_or(|id| id == event.agent_id)
    }
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
    /// Durable record for a background agent, when one was written. The
    /// manager settles it on completion, cancellation and drop.
    pub job_record: Option<(JobStore, String)>,
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
            job_record: None,
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
///
/// Shared (`Arc`) rather than owned so the manager can hand it to the spawned
/// task that actually completes an agent; the task runs independently of the
/// manager and cannot borrow it.
pub type TeammateEventCallback = Arc<dyn Fn(TeammateEvent) + Send + Sync + 'static>;

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
    /// Durable records for background agents this manager has already
    /// written. Kept so `Drop` can settle work the process is abandoning.
    background_jobs: Vec<(JobStore, String)>,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("agents", &self.agents.len())
            .field("custom_agents", &self.custom_agents.len())
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
            api_client: None,
            model: ModelId::sonnet(),
            event_tx: None,
            permission_checker: None,
            confirmation_bus: None,
            tool_executor_factory: None,
            agent_event_broadcast: Some(agent_event_broadcast),
            custom_agents: crate::custom_agents::load_custom_agents(),
            test_mode: false,
            background_jobs: Vec::new(),
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
        self.event_callback = Some(Arc::new(callback));
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
    pub const fn enable_test_mode(&mut self) {
        self.test_mode = true;
    }

    /// Subscribe to one agent's observable events.
    ///
    /// The returned [`AgentSubscription`] filters the manager-wide broadcast
    /// down to `agent_id`, so a subscriber does not have to match on the id
    /// itself. The special id `"all"` keeps the manager-wide behaviour and is
    /// equivalent to [`Self::subscribe_all`].
    #[must_use]
    pub fn subscribe_to_agent(&self, agent_id: &str) -> Option<AgentSubscription> {
        let filter = (agent_id != "all").then(|| agent_id.to_string());
        self.agent_event_broadcast
            .as_ref()
            .map(|tx| AgentSubscription {
                agent_id: filter,
                rx: tx.subscribe(),
            })
    }

    /// Subscribe to observable events from every agent.
    ///
    /// This is the manager-wide arm of [`Self::subscribe_to_agent`]; the
    /// special id `"all"` is what turns the filter off, so both entry points
    /// share one implementation.
    #[must_use]
    pub fn subscribe_all(&self) -> Option<AgentSubscription> {
        self.subscribe_to_agent("all")
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
        emit_teammate_event(self.event_callback.as_ref(), event);
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

        // Build forked context. When the caller supplies the parent's
        // conversation prefix, the fork reuses it verbatim (cache-aware fork);
        // otherwise the fork starts a fresh prefix.
        let mut context = ForkedContext::new(
            parent_session_id.to_string(),
            cwd.to_string(),
            message_count,
        )
        .with_tools(options.allowed_tools.clone().unwrap_or_default())
        .with_env(options.extra_env.clone())
        .with_conversation_prefix(options.parent_prefix.clone());
        if let Some(params) = options.parent_cache_safe_params.clone() {
            context = context.with_cache_safe_params(params);
        }

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

        // A background agent gets a durable record before its task starts, so
        // a process that dies at any point after this leaves something for the
        // next session to find instead of an invisible job. Synchronous agents
        // are not recorded: their call is already blocking the caller, and
        // there is no background work to adopt.
        let started_at_ms = now_ms();
        let background_job = if options.run_in_background {
            // The record lives in the session's store (the `cwd` argument),
            // even when the agent itself runs in a worktree or an overridden
            // directory. Adoption starts from the session's cwd, so a record
            // written elsewhere would never be found.
            let store = JobStore::new(cwd);
            let id = job_record_id("agent", &task_id, started_at_ms);
            let description =
                job_description(&format!("{}: ", agent_type_name(&agent_type)), prompt);
            let mut record = JobRecord::new(
                id.clone(),
                JobKind::Agent,
                description,
                &working_directory,
                Some(parent_session_id),
                started_at_ms,
            );
            // In-process work: the owner process is the only pid there is.
            record.pid = Some(std::process::id());
            if let Err(error) = store.write(&record) {
                tracing::warn!(
                    job = %id,
                    path = %store.root().display(),
                    error = %error,
                    "could not write the background agent job record"
                );
            }
            self.background_jobs.push((store.clone(), id.clone()));
            Some((store, id))
        } else {
            None
        };

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
        instance.job_record = background_job.clone();
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
                .map_or_else(|| self.model.clone(), |m| ModelId(m.clone()));
            let max_tokens = options.max_tokens.unwrap_or(4096);
            let max_duration = options.max_duration_seconds.unwrap_or(600);
            let max_turns = options.max_turns.unwrap_or(50);
            let allowed_tools = options.allowed_tools.clone();
            let writable = options.writable;
            let sandbox_config = options.sandbox.clone();
            let retry_policy = options.retry_policy.clone();
            let output_schema = options.output_schema.clone();
            let run_in_background = options.run_in_background;
            let user_prompt = prompt.to_string();
            let agent_subject = job_description("", prompt);
            let output_file_path = output_file.clone();
            let agent_id_for_task = agent_id.clone();
            let event_tx = self.event_tx.clone();
            let tool_factory = self.tool_executor_factory.clone();
            let broadcast_tx = self.agent_event_broadcast.clone();
            let confirmation_bus = self.confirmation_bus.clone();
            let status_tx = instance.status_tx.clone();
            let task_manager_for_task = Arc::clone(&self.task_manager);
            let task_id_for_completion = agent_id.clone();
            let job_record = background_job.clone();
            // The forked context and the teammate callback both belong to the
            // manager, but completion happens in the spawned task. Clone them
            // now: the task must be able to seed the child session and report
            // its real completion without borrowing the manager.
            let forked_context = instance.context.clone();
            let event_callback = self.event_callback.clone();

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
                    run_in_background,
                    forked_context,
                    event_callback,
                    agent_subject,
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

                // The record must never be left claiming `Running` once the
                // task is over. `settle` refuses to overwrite an already
                // terminal record, so a cancellation that raced completion
                // keeps its `Interrupted` state.
                if let Some((store, id)) = &job_record {
                    let (state, error) = match &result {
                        Ok(agent_result) if agent_result.status == AgentStatus::Completed => {
                            (JobState::Done, None)
                        }
                        Ok(_) => (JobState::Failed, Some("agent execution failed".to_string())),
                        Err(error) => (JobState::Failed, Some(error.to_string())),
                    };
                    if let Err(write_error) = store.settle(id, state, now_ms(), None, error) {
                        tracing::warn!(
                            job = %id,
                            path = %store.root().display(),
                            error = %write_error,
                            "could not settle the background agent job record"
                        );
                    }
                }
            });

            instance.task_join_handle = Some(handle);
        } else if cfg!(test) || self.test_mode {
            // Test convenience: write placeholder and complete immediately
            let placeholder = format!("agent completed (test mode)\n\nAgent task: {prompt}");
            let _ = tokio::fs::write(&output_file, &placeholder).await;
            let _ = instance.status_tx.send(AgentStatus::Completed);
            if let Some((store, id)) = &background_job {
                let _ = store.settle(id, JobState::Done, now_ms(), None, None);
            }
            // Test mode completes synchronously, but it must report through
            // the same teammate surface a real completion does.
            self.emit_event(TeammateEvent::Idle {
                agent_id: agent_id.clone(),
                reason: "turn complete".to_string(),
            });
            self.emit_event(TeammateEvent::TaskCompleted {
                agent_id: agent_id.clone(),
                subject: job_description("", prompt),
                summary: placeholder,
            });
        } else {
            let error = "AgentManager requires an API client to spawn agents. \
                 Configure one via set_api_client() before calling spawn_agent().";
            if let Some((store, id)) = &background_job {
                let _ = store.settle(
                    id,
                    JobState::Failed,
                    now_ms(),
                    None,
                    Some(error.to_string()),
                );
            }
            return Err(AgentError::Configuration(error.into()));
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
    //
    // Kept `async` so callers have one manager surface; delivery is a
    // best-effort channel send today.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn send_message(&self, agent_id: &str, content: &str) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| AgentError::NotFound(agent_id.to_string()))?;

        if agent.status() != AgentStatus::Running {
            return Err(AgentError::NotRunning(agent_id.to_string()));
        }

        let Some(tx) = agent.message_tx.as_ref() else {
            return Err(AgentError::NotRunning(agent_id.to_string()));
        };
        let msg = AgentMessage {
            agent_id: agent_id.to_string(),
            content: content.to_string(),
            message_id: None,
        };
        // Best-effort send — in a real implementation the receiver would be
        // held by the agent process. For now, we ignore closed-channel errors
        // (the receiver may not yet be connected).
        let _ = tx.send(msg);
        Ok(())
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
        if let Some((store, id)) = &instance.job_record {
            let _ = store.settle(id, JobState::Done, now_ms(), None, None);
        }

        // Emit teammate event
        self.emit_event(TeammateEvent::TaskCompleted {
            agent_id: agent_id.to_string(),
            subject: subject.to_string(),
            summary: summary.to_string(),
        });

        Ok(())
    }

    /// Mark an agent as idle and emit an Idle event.
    //
    // Kept `async` for the same reason as `send_message`.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
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
        // Cancellation is a lifecycle end: the record must not stay
        // `Running`, or a later session would report work nobody is doing.
        if let Some((store, id)) = &instance.job_record {
            if let Err(error) = store.settle(
                id,
                JobState::Interrupted,
                now_ms(),
                None,
                Some("agent stopped before completion".to_string()),
            ) {
                tracing::warn!(
                    job = %id,
                    path = %store.root().display(),
                    error = %error,
                    "could not settle the cancelled agent job record"
                );
            }
        }
        // Drop the actual task as well as its status. A stopped background
        // agent must not keep running invisibly.
        if let Some(handle) = instance.task_join_handle.take() {
            handle.abort();
        }

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

    // ─── Custom (named) agents ──────────────────────────────────────────

    /// Replace the loaded custom agent definitions.
    ///
    /// Production managers load `~/.coderex/agents/*.json` in
    /// [`AgentManager::new`]. Tests and embedders use this to install a
    /// known set without touching the user's home directory.
    pub fn set_custom_agents(&mut self, agents: HashMap<String, CustomAgentDef>) {
        self.custom_agents = agents;
    }

    /// Add or replace one custom agent definition.
    pub fn register_custom_agent(&mut self, def: CustomAgentDef) {
        self.custom_agents.insert(def.name.clone(), def);
    }

    /// Look up a custom agent definition by name.
    #[must_use]
    pub fn custom_agent(&self, name: &str) -> Option<&CustomAgentDef> {
        self.custom_agents.get(name)
    }

    /// Whether a custom agent with this name is configured.
    #[must_use]
    pub fn has_custom_agent(&self, name: &str) -> bool {
        self.custom_agents.contains_key(name)
    }

    /// Names of all configured custom agents, sorted for stable output.
    #[must_use]
    pub fn custom_agent_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.custom_agents.keys().cloned().collect();
        names.sort();
        names
    }

    /// Spawn a configured custom agent by name.
    ///
    /// This is the production entry point for named agents: it resolves the
    /// [`CustomAgentDef`], reports an error listing the available names when
    /// the lookup fails, and delegates to [`AgentManager::spawn_agent`] with
    /// [`AgentType::Custom`]. `spawn_agent` merges the definition's system
    /// prompt, tool whitelist, model, token limit and output schema into the
    /// spawn.
    ///
    /// # Errors
    /// Returns [`AgentError::UnknownCustomAgent`] when no definition matches.
    pub async fn spawn_named_agent(
        &mut self,
        name: &str,
        prompt: &str,
        options: SpawnOptions,
        parent_session_id: &str,
        cwd: &str,
        message_count: usize,
    ) -> Result<&AgentInstance, AgentError> {
        if !self.custom_agents.contains_key(name) {
            return Err(AgentError::UnknownCustomAgent {
                name: name.to_string(),
                available: available_agents_description(&self.custom_agent_names()),
            });
        }

        self.spawn_agent(
            AgentType::Custom {
                prompt: name.to_string(),
            },
            prompt,
            options,
            parent_session_id,
            cwd,
            message_count,
        )
        .await
    }

    // ─── Background Agent Execution ──────────────────────────────────────────

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

impl Drop for AgentManager {
    fn drop(&mut self) {
        // A manager going away means its tokio tasks are going away too. Mark
        // any still-running background record interrupted so a later session
        // reports the loss instead of finding a phantom `Running`.
        for (store, id) in &self.background_jobs {
            let _ = store.settle(
                id,
                JobState::Interrupted,
                now_ms(),
                None,
                Some("agent manager dropped before the job finished".to_string()),
            );
        }
    }
}

/// Build a job id that is safe as a file stem and does not collide with a
/// previous session's `task_0`. Task ids restart at zero in every process, so
/// the start timestamp is part of the id.
fn job_record_id(prefix: &str, task_id: &str, started_at_ms: i64) -> String {
    let sanitized: String = task_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "job".to_string()
    } else {
        sanitized
    };
    format!("{prefix}-{sanitized}-{}", started_at_ms.max(0))
}

/// A bounded, human-readable description for a job record.
fn job_description(prefix: &str, text: &str) -> String {
    const LIMIT: usize = 200;
    let mut out = String::with_capacity(prefix.len() + text.len().min(LIMIT) + 1);
    out.push_str(prefix);
    let mut chars = text.chars();
    for _ in 0..LIMIT {
        match chars.next() {
            Some(c) => out.push(c),
            None => return out,
        }
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// The only `SpawnOptions::isolation` value this manager can honor.
const ISOLATION_WORKTREE: &str = "worktree";

// ─── Agent Event Logs ────────────────────────────────────────────────────────

/// Directory where per-agent NDJSON event logs are written.
///
/// The writer in [`run_agent_task`] and the reader [`read_agent_log`] share
/// this one definition, so a log written by a session is always found by a
/// reader in a later one.
#[must_use]
pub fn agent_log_dir() -> PathBuf {
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".coderex")
        .join("agent_logs")
}

/// Path to one agent's NDJSON event log.
#[must_use]
pub fn agent_log_path(agent_id: &str) -> PathBuf {
    agent_log_dir().join(format!("{agent_id}.jsonl"))
}

/// Read and decode an agent's NDJSON event log.
///
/// Events are returned in the order they were written. A missing file is an
/// empty log — an agent that produced no events — rather than an error, so a
/// caller can ask about any id without first checking whether it ran.
pub fn read_agent_log(agent_id: &str) -> Result<Vec<WireEvent>, AgentError> {
    read_agent_log_at(&agent_log_path(agent_id))
}

/// Read and decode an NDJSON event log from an explicit path.
///
/// Blank lines are skipped. A line that is not a valid event is a hard error:
/// a truncated or corrupt log must be reported, not silently shortened, or a
/// reader would believe it had seen the whole run.
pub fn read_agent_log_at(path: &Path) -> Result<Vec<WireEvent>, AgentError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AgentError::Configuration(format!(
                "could not read agent log {}: {error}",
                path.display()
            )));
        }
    };

    let mut events = Vec::new();
    for (index, line) in content.lines().enumerate() {
        match decode_event(line) {
            None => {}
            Some(Ok(event)) => events.push(event),
            Some(Err(error)) => {
                return Err(AgentError::Configuration(format!(
                    "malformed agent log line {} in {}: {error}",
                    index + 1,
                    path.display()
                )));
            }
        }
    }
    Ok(events)
}

/// Reject spawn options this manager cannot honor.
///
/// Both checks guard against the same failure mode: a request that is
/// accepted and then silently ignored. A caller that asked for worktree
/// isolation believes the agent cannot touch their checkout; a caller that
/// asked for environment variables believes the agent has them. Running the
/// agent anyway, without either, is worse than refusing to spawn it.
///
/// `extra_env` is refused because there is nowhere honest to put it. A
/// sub-agent runs in this process, so it has no child environment of its own.
/// The two surfaces a per-call variable could ride on do not exist in this
/// build: `ToolContext` (the object every tool call receives) carries
/// `execution_context`, `cwd`, `extra` and `elicit_tx` but no environment map,
/// and `SessionState` carries no environment either. The only working
/// alternative is `std::env::set_var`, which is process-wide: it would leak
/// into the parent and every other agent, the exact opposite of the
/// isolation the caller asked for. So the request is refused, not faked.
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
             in the parent process, so there is no child environment to inject into. Neither \
             `ToolContext` nor `SessionState` carries a per-call environment, and setting \
             process-wide variables with `std::env::set_var` would leak into the parent and other \
             agents; refusing the spawn instead of ignoring the request"
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
    run_in_background: bool,
    forked_context: Option<ForkedContext>,
    event_callback: Option<TeammateEventCallback>,
    agent_subject: String,
    msg_rx: &mut mpsc::UnboundedReceiver<AgentMessage>,
) -> Result<AgentResult, AgentError> {
    use std::fmt::Write as _;
    use tokio::io::AsyncWriteExt as _;

    // Notify parent that the agent started
    emit_lifecycle(
        parent_event_tx.as_ref(),
        &agent_id,
        AgentLifecycleEvent::Started,
    );

    // `on_failure` decides whether a failed attempt is retried at all.
    // `retry` re-runs the attempt up to `max_retries`; `escalate_to_parent`
    // stops at the first failure and marks the result as escalated, even if
    // `max_retries` is non-zero.
    let on_failure = retry_policy
        .as_ref()
        .map_or(FailureAction::EscalateToParent, |r| r.on_failure);
    let max_retries = if matches!(on_failure, FailureAction::Retry) {
        retry_policy.as_ref().map_or(0, |r| r.max_retries)
    } else {
        0
    };
    let escalation_prompt = retry_policy
        .as_ref()
        .and_then(|r| r.escalation_prompt.clone());

    // Set up agent log file. The directory and path come from the same
    // helpers `read_agent_log` uses, so the writer and reader cannot drift.
    let log_dir = agent_log_dir();
    let log_path = agent_log_path(&agent_id);

    let fork_task = user_prompt.clone();
    let mut attempt_prompt = user_prompt;
    let mut final_output = String::new();
    let mut final_status = AgentStatus::Completed;
    let mut final_error: Option<String> = None;
    let mut total_token_usage: Option<u64> = None;
    // Whether the child request's system prompt/model/tool set matches the
    // parent's, i.e. whether the reused prefix can actually hit the provider
    // cache. Recorded in the meta file so the decision is observable.
    let mut cache_safe: Option<bool> = None;

    for attempt in 0..=max_retries {
        // Drain any messages sent by the parent before this attempt
        while let Ok(msg) = msg_rx.try_recv() {
            let _ = write!(attempt_prompt, "\n\n[Message from parent]: {}", msg.content);
        }

        // Seed the child session with the parent's conversation prefix. The
        // engine appends the task itself in `submit`, so this is exactly
        // `ForkedContext::fork_messages(task)` at the provider layer.
        let mut session = SessionState::new(working_directory.clone());
        let mut seeded_prefix_messages = 0usize;
        if let Some(ref fork) = forked_context {
            match seed_session_from_fork(&mut session, fork, &fork_task) {
                Ok(seeded) => seeded_prefix_messages = seeded,
                Err(error) => tracing::warn!(
                    agent_id = %agent_id,
                    error = %error,
                    "could not reuse the parent prefix; starting the child from a fresh prefix"
                ),
            }
        }

        let checker = PermissionChecker::new(permission_mode);
        let mut permission_engine = PermissionEngine::new(checker);
        // A background child runs with `agent_id = Some(..)`, which makes its
        // permission requests background-agent requests. The hook is installed
        // for every child because it is a no-op for an interactive run.
        permission_engine.add_hook(Arc::new(BackgroundAgentDenyHook));

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

        let mut advertised_tool_names = Vec::new();
        if let Some(ref factory) = tool_factory {
            let executor = factory(working_directory.clone(), sandbox_config.clone());
            advertised_tool_names = executor
                .list_tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect();
            engine.set_tool_executor(executor);
        }

        if let Some(ref fork) = forked_context {
            if fork.reuses_parent_prefix() {
                let safe = fork.is_cache_safe(&model.0, &system_prompt, &advertised_tool_names);
                cache_safe = Some(safe);
                tracing::debug!(
                    agent_id = %agent_id,
                    prefix_messages = seeded_prefix_messages,
                    prefix_fingerprint = %fork.prefix_fingerprint(),
                    cache_safe = safe,
                    "seeded the child session from the parent prefix"
                );
            }
        }

        // Only an agent the person is not watching is a background run. A
        // synchronous sub-agent waits on its parent's turn, so its permission
        // requests stay in the interactive path; tagging every child with an
        // id made `permission_context_for` call them all background and the
        // deny hook refused even read-only tools.
        let options = SubmitOptions {
            agent_id: run_in_background.then(|| agent_id.clone()),
            custom_system_prompt: Some(system_prompt.clone()),
            allowed_tools_override: effective_allowed_tools(allowed_tools.clone(), writable),
            max_turns,
            task_budget_tokens: u64::from(max_tokens),
            ..SubmitOptions::default()
        };

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
                let _ = write!(attempt_prompt, "\n\n[Message from parent]: {}", msg.content);
            }

            match &event {
                EngineEvent::TextDelta(text) => {
                    attempt_output.push_str(text);
                    // Let the parent observe the sub-agent's output as it is
                    // produced, rather than only in the final Completed
                    // summary.
                    emit_lifecycle(
                        parent_event_tx.as_ref(),
                        &agent_id,
                        AgentLifecycleEvent::TextDelta(text.clone()),
                    );
                }
                EngineEvent::ToolCallStart { name, summary, .. } => {
                    // The engine's start event carries the activity summary,
                    // not the parsed tool input (the input never leaves the
                    // engine as a standalone event). Surface the summary here
                    // so a subscriber still gets a usable payload.
                    emit_lifecycle(
                        parent_event_tx.as_ref(),
                        &agent_id,
                        AgentLifecycleEvent::ToolCall {
                            name: name.clone(),
                            args: serde_json::json!({ "summary": summary }),
                        },
                    );
                }
                _ => {}
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
                        file.write_all(json.as_bytes()).await?;
                        file.write_all(b"\n").await?;
                        Ok::<(), std::io::Error>(())
                    }
                    .await;
                }
            }
        }

        // The turn's work is done: the engine returned and its event stream is
        // drained. Report idleness before the terminal Completed/Failed event
        // so observers can distinguish "waiting for more work" from "finished".
        emit_lifecycle(
            parent_event_tx.as_ref(),
            &agent_id,
            AgentLifecycleEvent::Idle {
                reason: "turn complete".to_string(),
            },
        );
        // The same real idle point feeds the manager-level teammate callback,
        // so observers registered through `set_event_callback` see it too.
        emit_teammate_event(
            event_callback.as_ref(),
            TeammateEvent::Idle {
                agent_id: agent_id.clone(),
                reason: "turn complete".to_string(),
            },
        );

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
                            let err_msg = format!("validation failed: {error}");
                            let (output, error) = final_failure_output(
                                &attempt_output,
                                &err_msg,
                                on_failure,
                                max_retries,
                            );
                            final_output = output;
                            final_error = Some(error);
                            final_status = AgentStatus::Failed;
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
                        let _ = write!(retry_note, "\n{ep}");
                    }
                    attempt_prompt.push_str(&retry_note);
                    continue;
                }
                let (output, error) =
                    final_failure_output(&attempt_output, &err_msg, on_failure, max_retries);
                final_output = output;
                final_error = Some(error);
                final_status = AgentStatus::Failed;
            }
            Err(_) => {
                let err_msg = format!("agent timed out after {max_duration}s");
                if attempt < max_retries {
                    let mut retry_note = format!(
                        "\n\n[Previous attempt timed out after {max_duration}s. Retrying...]"
                    );
                    if let Some(ref ep) = escalation_prompt {
                        let _ = write!(retry_note, "\n{ep}");
                    }
                    attempt_prompt.push_str(&retry_note);
                    continue;
                }
                let (output, error) =
                    final_failure_output(&attempt_output, &err_msg, on_failure, max_retries);
                final_output = output;
                final_error = Some(error);
                final_status = AgentStatus::Failed;
            }
        }
    }

    let (status, error) = (final_status, final_error);

    // Notify parent of completion/failure
    let lifecycle = error.as_ref().map_or_else(
        || AgentLifecycleEvent::Completed {
            summary: final_output.clone(),
        },
        |err| AgentLifecycleEvent::Failed { error: err.clone() },
    );
    emit_lifecycle(parent_event_tx.as_ref(), &agent_id, lifecycle);

    // A normally completing agent is the production completion path: report
    // it through the manager's teammate callback in addition to the parent
    // engine event stream.
    if status == AgentStatus::Completed {
        emit_teammate_event(
            event_callback.as_ref(),
            TeammateEvent::TaskCompleted {
                agent_id: agent_id.clone(),
                subject: agent_subject,
                summary: final_output.clone(),
            },
        );
    }

    // Write output to file
    let _ = tokio::fs::write(&output_file, &final_output).await;

    // Write companion meta file with token usage, final status and the
    // fork's cache decision. A reused prefix whose request parameters differ
    // is still a valid conversation fork, but it is not a cache hit — saying
    // so here keeps the distinction observable.
    let meta_path = output_file.with_extension("meta.json");
    let fork_meta = forked_context.as_ref().map(|fork| {
        serde_json::json!({
            "reused": fork.reuses_parent_prefix(),
            "prefix_messages": fork.fork_point,
            "prefix_fingerprint": fork.prefix_fingerprint(),
            "cache_safe": cache_safe.unwrap_or(false),
        })
    });
    let meta = serde_json::json!({
        "status": status,
        "token_usage": total_token_usage,
        "error": error,
        "escalated": matches!(on_failure, FailureAction::EscalateToParent)
            && status == AgentStatus::Failed,
        "fork": fork_meta,
    });
    let _ = tokio::fs::write(&meta_path, meta.to_string()).await;

    Ok(AgentResult {
        output: final_output,
        status,
        token_usage: total_token_usage,
    })
}

/// Invoke a teammate callback, if one is registered.
fn emit_teammate_event(callback: Option<&TeammateEventCallback>, event: TeammateEvent) {
    if let Some(callback) = callback {
        callback(event);
    }
}

/// Build the terminal output and error text for a failed attempt.
///
/// The marker distinguishes the two advertised policies: `retry` used up its
/// attempts, while `escalate_to_parent` stopped and leaves the decision to
/// the caller.
fn final_failure_output(
    attempt_output: &str,
    error: &str,
    on_failure: FailureAction,
    max_retries: u32,
) -> (String, String) {
    let marker = match on_failure {
        FailureAction::Retry => format!("[retries exhausted after {max_retries}] {error}"),
        FailureAction::EscalateToParent => format!("[escalated to parent] {error}"),
    };
    if attempt_output.is_empty() {
        (marker.clone(), marker)
    } else {
        (format!("{marker}\n\n{attempt_output}"), marker)
    }
}

/// Seed a child session with a fork's parent prefix.
///
/// [`ForkedContext::fork_messages`] describes the child's full conversation:
/// the parent prefix followed by the sub-agent's task. `QueryEngine::submit`
/// appends the task itself, so the session is seeded with everything before
/// that final entry. Returns the number of parent messages seeded.
///
/// A prefix that cannot be expressed as engine messages is rejected as a
/// whole rather than partially seeded: reusing a truncated conversation would
/// present a prefix that never existed.
fn seed_session_from_fork(
    session: &mut SessionState,
    context: &ForkedContext,
    task: &str,
) -> Result<usize, String> {
    if !context.reuses_parent_prefix() {
        return Ok(0);
    }

    let forked = context.fork_messages(task);
    let prefix_len = context.fork_point.min(forked.len());
    let messages = messages_from_fork_prefix(&forked[..prefix_len])?;
    let seeded = messages.len();
    session.messages = messages;
    Ok(seeded)
}

/// Convert a stored parent prefix into engine messages.
fn messages_from_fork_prefix(values: &[serde_json::Value]) -> Result<Vec<Message>, String> {
    values.iter().map(message_from_fork_value).collect()
}

/// Convert one stored prefix message.
///
/// Production snapshots serialize the engine's own [`Message`], which is the
/// canonical form and is parsed first. API/provider-shaped JSON — the shape
/// [`ForkedContext::fork_messages`] documents — is accepted as a fallback so a
/// caller that captured provider messages can still fork.
fn message_from_fork_value(value: &serde_json::Value) -> Result<Message, String> {
    if let Ok(message) = serde_json::from_value::<Message>(value.clone()) {
        return Ok(message);
    }

    let role = value
        .get("role")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("message has no string role: {value}"))?;
    let role = role_from_name(role)?;
    let content = value
        .get("content")
        .ok_or_else(|| format!("message has no content: {value}"))?;

    match content {
        serde_json::Value::String(text) => Ok(Message {
            role,
            content: MessageContent::Text(text.clone()),
        }),
        serde_json::Value::Array(blocks) => message_from_blocks(role, blocks),
        other => Err(format!("unsupported message content: {other}")),
    }
}

fn role_from_name(name: &str) -> Result<Role, String> {
    match name.to_ascii_lowercase().as_str() {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(format!("unknown message role {other:?}")),
    }
}

fn message_from_blocks(role: Role, blocks: &[serde_json::Value]) -> Result<Message, String> {
    if let Some(text) = blocks_text(blocks) {
        return Ok(Message {
            role,
            content: MessageContent::Text(text),
        });
    }

    if let [block] = blocks {
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| format!("tool_use block has no id: {block}"))?;
                let name = block
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| format!("tool_use block has no name: {block}"))?;
                return Ok(Message {
                    role: Role::Assistant,
                    content: MessageContent::ToolCall(ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        input: block.get("input").cloned().unwrap_or_default(),
                    }),
                });
            }
            Some("tool_result") => {
                let tool_call_id = block
                    .get("tool_use_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| format!("tool_result block has no tool_use_id: {block}"))?;
                let output = match block.get("content") {
                    Some(serde_json::Value::String(text)) => text.clone(),
                    Some(serde_json::Value::Array(items)) => {
                        blocks_text(items).unwrap_or_else(|| {
                            block
                                .get("content")
                                .map(ToString::to_string)
                                .unwrap_or_default()
                        })
                    }
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let is_error = block
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                return Ok(Message {
                    role: Role::Tool,
                    content: MessageContent::ToolResult(ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output,
                        is_error,
                        images: Vec::new(),
                    }),
                });
            }
            _ => {}
        }
    }

    Err(format!(
        "cannot represent the message's content blocks as an engine message: {blocks:?}"
    ))
}

/// Concatenate a block list made up entirely of text blocks.
fn blocks_text(blocks: &[serde_json::Value]) -> Option<String> {
    if blocks.is_empty() {
        return None;
    }
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("text") {
            return None;
        }
        text.push_str(block.get("text").and_then(serde_json::Value::as_str)?);
    }
    Some(text)
}

/// Send a lifecycle event to the parent engine, if one is attached.
fn emit_lifecycle(
    tx: Option<&mpsc::UnboundedSender<EngineEvent>>,
    agent_id: &str,
    event: AgentLifecycleEvent,
) {
    if let Some(tx) = tx {
        let _ = tx.send(EngineEvent::AgentEvent {
            agent_id: agent_id.to_string(),
            event,
        });
    }
}

/// Tools a read-only (`writable = false`) agent may use.
const READ_ONLY_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

/// Narrow a tool whitelist for a read-only agent.
///
/// `None` means "no explicit whitelist", which for a writable agent leaves the
/// full tool set available. For a read-only agent it becomes the read-only
/// set; for an explicit whitelist, entries that are not read-only are
/// dropped. The engine enforces the result through
/// `SubmitOptions::allowed_tools_override`.
fn effective_allowed_tools(
    allowed_tools: Option<Vec<String>>,
    writable: bool,
) -> Option<Vec<String>> {
    if writable {
        return allowed_tools;
    }

    Some(allowed_tools.map_or_else(
        || {
            READ_ONLY_TOOLS
                .iter()
                .map(|tool| (*tool).to_string())
                .collect()
        },
        |tools| {
            tools
                .into_iter()
                .filter(|tool| {
                    READ_ONLY_TOOLS
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(tool))
                })
                .collect()
        },
    ))
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

    fn observable(agent_id: &str, text: &str) -> AgentObservableEvent {
        AgentObservableEvent {
            agent_id: agent_id.to_string(),
            event: EngineEvent::TextDelta(text.to_string()),
        }
    }

    #[tokio::test]
    async fn per_agent_subscriptions_only_see_their_agent() {
        let mgr = make_manager();
        let mut first = mgr.subscribe_to_agent("agent-1").unwrap();
        let mut second = mgr.subscribe_to_agent("agent-2").unwrap();
        let tx = mgr
            .agent_event_broadcast
            .as_ref()
            .expect("manager has a broadcast channel");

        tx.send(observable("agent-1", "one")).unwrap();
        tx.send(observable("agent-2", "two")).unwrap();
        tx.send(observable("agent-1", "three")).unwrap();

        let first_event = first.try_recv().expect("agent-1's first event");
        assert_eq!(first_event.agent_id, "agent-1");
        assert!(matches!(
            first_event.event,
            EngineEvent::TextDelta(ref text) if text == "one"
        ));
        let first_again = first.try_recv().expect("agent-1's second event");
        assert_eq!(first_again.agent_id, "agent-1");
        assert!(
            first.try_recv().is_err(),
            "agent-1's subscriber saw another agent's event"
        );

        let second_event = second.try_recv().expect("agent-2's event");
        assert_eq!(second_event.agent_id, "agent-2");
        assert!(
            second.try_recv().is_err(),
            "agent-2's subscriber saw another agent's event"
        );
    }

    #[tokio::test]
    async fn subscribe_all_receives_every_agent() {
        let mgr = make_manager();
        let mut all = mgr.subscribe_all().unwrap();
        let tx = mgr
            .agent_event_broadcast
            .as_ref()
            .expect("manager has a broadcast channel");

        tx.send(observable("agent-1", "one")).unwrap();
        tx.send(observable("agent-2", "two")).unwrap();

        let first = all.recv().await.unwrap();
        let second = all.recv().await.unwrap();
        assert_eq!(first.agent_id, "agent-1");
        assert_eq!(second.agent_id, "agent-2");
    }

    #[tokio::test]
    async fn the_all_alias_keeps_the_manager_wide_subscription() {
        let mgr = make_manager();
        let mut all = mgr.subscribe_to_agent("all").unwrap();
        assert!(all.agent_id().is_none());
        let tx = mgr
            .agent_event_broadcast
            .as_ref()
            .expect("manager has a broadcast channel");

        tx.send(observable("agent-1", "one")).unwrap();
        tx.send(observable("agent-2", "two")).unwrap();

        assert_eq!(all.recv().await.unwrap().agent_id, "agent-1");
        assert_eq!(all.recv().await.unwrap().agent_id, "agent-2");
    }

    // ─── Agent event log reader ─────────────────────────────────────────────

    /// The writer in `run_agent_task` and `read_agent_log` must agree on the
    /// location, or a log written by one session is invisible to the next.
    #[test]
    fn agent_log_path_is_under_the_shared_log_dir() {
        let path = agent_log_path("agent-7");
        assert_eq!(path.file_name().unwrap(), "agent-7.jsonl");
        assert_eq!(path.parent().unwrap(), agent_log_dir());
    }

    #[test]
    fn read_agent_log_reads_written_events_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-1.jsonl");
        let first = WireEvent::TextDelta {
            delta: "one".to_string(),
        };
        let second = WireEvent::Notification {
            message: "two".to_string(),
        };
        let mut body = coderex_core::wire_protocol::encode_event(&first);
        body.push_str(&coderex_core::wire_protocol::encode_event(&second));
        std::fs::write(&path, body).unwrap();

        let events = read_agent_log_at(&path).unwrap();
        assert_eq!(events, vec![first, second]);
    }

    #[test]
    fn read_agent_log_of_a_missing_agent_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-ran.jsonl");
        assert!(read_agent_log_at(&path).unwrap().is_empty());
    }

    #[test]
    fn read_agent_log_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-2.jsonl");
        let event = WireEvent::TextDelta {
            delta: "kept".to_string(),
        };
        let body = format!(
            "\n{}\n\n",
            coderex_core::wire_protocol::encode_event(&event).trim_end()
        );
        std::fs::write(&path, body).unwrap();

        assert_eq!(read_agent_log_at(&path).unwrap(), vec![event]);
    }

    #[test]
    fn read_agent_log_rejects_a_corrupt_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-3.jsonl");
        let good = coderex_core::wire_protocol::encode_event(&WireEvent::TextDelta {
            delta: "ok".to_string(),
        });
        std::fs::write(&path, format!("{good}{{not json}}\n")).unwrap();

        let error = read_agent_log_at(&path).unwrap_err();
        assert!(
            matches!(error, AgentError::Configuration(ref msg) if msg.contains("line 2")),
            "corrupt log should name the bad line: {error:?}"
        );
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

    /// Create a git repository with one commit so `git worktree add` can run.
    fn init_git_repo(dir: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = coderex_core::git_hardening::git_command()
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
        // Refusing must not be done by leaking the variable into this process
        // as a side effect of "trying": the parent environment is untouched.
        assert!(std::env::var("SECRET").is_err());
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

    fn parent_prefix() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"role": "user", "content": "first question"}),
            serde_json::json!({"role": "assistant", "content": "first answer"}),
            serde_json::json!({"role": "user", "content": "second question"}),
        ]
    }

    #[test]
    fn forked_context_reuses_parent_prefix_verbatim() {
        let prefix = parent_prefix();
        let ctx = ForkedContext::new("parent".into(), "/src".into(), 3)
            .with_conversation_prefix(prefix.clone());

        assert!(ctx.reuses_parent_prefix());
        assert_eq!(ctx.fork_point, 3);

        let messages = ctx.fork_messages("do the sub-task");
        // The parent prefix is reused byte-for-byte (a fresh prefix would not
        // hit the provider's cache), and the task follows the fork point.
        assert_eq!(messages.len(), 4);
        assert_eq!(&messages[..3], prefix.as_slice());
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "do the sub-task");
    }

    #[test]
    fn fork_without_prefix_starts_fresh() {
        let ctx = ForkedContext::new("parent".into(), "/src".into(), 0);
        assert!(!ctx.reuses_parent_prefix());
        assert_eq!(ctx.fork_point, 0);

        let messages = ctx.fork_messages("task");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "task");
    }

    #[test]
    fn forked_context_prefix_fingerprint_detects_drift() {
        let base = ForkedContext::new("parent".into(), "/src".into(), 3)
            .with_conversation_prefix(parent_prefix());

        let mut changed_prefix = parent_prefix();
        changed_prefix[1]["content"] = serde_json::json!("a different answer");
        let changed = ForkedContext::new("parent".into(), "/src".into(), 3)
            .with_conversation_prefix(changed_prefix);

        let same = ForkedContext::new("parent".into(), "/src".into(), 3)
            .with_conversation_prefix(parent_prefix());

        assert_eq!(base.prefix_fingerprint(), same.prefix_fingerprint());
        assert_ne!(base.prefix_fingerprint(), changed.prefix_fingerprint());
    }

    #[test]
    fn forked_context_cache_safety_requires_matching_params() {
        let tools = vec!["Read".to_string(), "Grep".to_string()];
        let params = CacheSafeParams::new("claude-sonnet-4-6", "you are helpful", tools.clone());
        let ctx = ForkedContext::new("parent".into(), "/src".into(), 1)
            .with_conversation_prefix(parent_prefix())
            .with_cache_safe_params(params);

        assert!(ctx.is_cache_safe("claude-sonnet-4-6", "you are helpful", &tools));
        // A different system prompt or tool set breaks the cache.
        assert!(!ctx.is_cache_safe("claude-sonnet-4-6", "you are a sub-agent", &tools));
        assert!(!ctx.is_cache_safe("claude-opus-4-1", "you are helpful", &tools));
        assert!(!ctx.is_cache_safe("claude-sonnet-4-6", "you are helpful", &["Read".into()]));

        // Without recorded params there is nothing to be cache-safe against.
        let bare = ForkedContext::new("parent".into(), "/src".into(), 1);
        assert!(!bare.is_cache_safe("claude-sonnet-4-6", "you are helpful", &tools));
    }

    #[tokio::test]
    async fn spawn_agent_records_parent_prefix() {
        let mut mgr = make_manager();
        let prefix = parent_prefix();
        let options = SpawnOptions {
            parent_prefix: prefix.clone(),
            parent_cache_safe_params: Some(CacheSafeParams::new("m", "sys", vec!["Read".into()])),
            ..Default::default()
        };

        let instance = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "sub-task",
                options,
                "sess_1",
                "/tmp",
                prefix.len(),
            )
            .await
            .unwrap();

        let ctx = instance.context.as_ref().unwrap();
        assert!(ctx.reuses_parent_prefix());
        assert_eq!(ctx.conversation_prefix, prefix);
        assert_eq!(ctx.fork_point, prefix.len());
        assert_eq!(ctx.fork_messages("sub-task").len(), prefix.len() + 1);
    }

    fn reviewer_def() -> CustomAgentDef {
        CustomAgentDef {
            name: "reviewer".into(),
            system_prompt: "You review code carefully.".into(),
            allowed_tools: vec!["Read".into(), "Grep".into()],
            model: Some("claude-test".into()),
            max_tokens: Some(1234),
            output_schema: None,
        }
    }

    #[tokio::test]
    async fn spawn_named_agent_uses_definition() {
        let mut mgr = make_manager();
        mgr.register_custom_agent(reviewer_def());

        let instance = mgr
            .spawn_named_agent(
                "reviewer",
                "review auth.rs",
                SpawnOptions::default(),
                "sess_named",
                "/tmp",
                0,
            )
            .await
            .unwrap();

        assert!(
            instance.prompt.contains("You review code carefully."),
            "system prompt was not applied: {}",
            instance.prompt
        );
        assert_eq!(
            instance.options.allowed_tools,
            Some(vec!["Read".into(), "Grep".into()])
        );
        assert_eq!(
            instance.options.model_override.as_deref(),
            Some("claude-test")
        );
        assert_eq!(instance.options.max_tokens, Some(1234));
        // The custom name is carried as the agent type.
        assert!(matches!(
            instance.agent_type,
            AgentType::Custom { ref prompt } if prompt == "reviewer"
        ));
    }

    #[tokio::test]
    async fn spawn_named_agent_unknown_lists_available_names() {
        let mut mgr = make_manager();
        mgr.register_custom_agent(reviewer_def());

        let error = mgr
            .spawn_named_agent(
                "missing",
                "do work",
                SpawnOptions::default(),
                "sess_named",
                "/tmp",
                0,
            )
            .await
            .unwrap_err();

        match error {
            AgentError::UnknownCustomAgent { name, available } => {
                assert_eq!(name, "missing");
                assert!(available.contains("reviewer"), "{available}");
            }
            other => panic!("expected UnknownCustomAgent, got {other:?}"),
        }
        assert!(mgr.list_agents().is_empty());
    }

    #[test]
    fn read_only_spawn_narrows_tools() {
        assert_eq!(
            effective_allowed_tools(None, false),
            Some(vec!["Read".into(), "Grep".into(), "Glob".into()])
        );
        assert_eq!(
            effective_allowed_tools(
                Some(vec!["Read".into(), "Write".into(), "Bash".into()]),
                false
            ),
            Some(vec!["Read".into()])
        );
        // A writable agent keeps its whitelist untouched.
        assert_eq!(
            effective_allowed_tools(Some(vec!["Read".into(), "Write".into()]), true),
            Some(vec!["Read".into(), "Write".into()])
        );
        assert_eq!(effective_allowed_tools(None, true), None);
    }

    struct ScriptedApi {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ApiClient for ScriptedApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                calls: Arc::clone(&self.calls),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> Result<coderex_core::api_client::ApiResponse, coderex_core::api_client::ApiError>
        {
            use coderex_core::api_client::ApiResponse;
            use coderex_core::types::UsageSummary;

            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let usage = UsageSummary {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            if call == 0 {
                Ok(ApiResponse {
                    content: String::new(),
                    tool_calls: vec![coderex_core::types::ToolCall {
                        id: "call_1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"file_path": "note.txt"}),
                    }],
                    usage,
                    stop_reason: "tool_use".into(),
                })
            } else {
                Ok(ApiResponse {
                    content: "done from the mock".into(),
                    tool_calls: Vec::new(),
                    usage,
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
            _tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = coderex_core::api_client::StreamEvent> + Send + 'static>,
        > {
            Box::pin(futures::stream::iter([
                coderex_core::api_client::StreamEvent::StreamEnd,
            ]))
        }
    }

    /// A client that answers immediately and records each request's messages.
    struct CapturingApi {
        requests: Arc<std::sync::Mutex<Vec<Vec<serde_json::Value>>>>,
    }

    #[async_trait::async_trait]
    impl ApiClient for CapturingApi {
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
            _tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> Result<coderex_core::api_client::ApiResponse, coderex_core::api_client::ApiError>
        {
            self.requests.lock().unwrap().push(messages.to_vec());
            Ok(coderex_core::api_client::ApiResponse {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: coderex_core::types::UsageSummary {
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
            _tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = coderex_core::api_client::StreamEvent> + Send + 'static>,
        > {
            Box::pin(futures::stream::iter([
                coderex_core::api_client::StreamEvent::StreamEnd,
            ]))
        }
    }

    /// A client that always fails with a non-retryable error.
    struct AlwaysFailApi {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ApiClient for AlwaysFailApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self {
                calls: Arc::clone(&self.calls),
            })
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> Result<coderex_core::api_client::ApiResponse, coderex_core::api_client::ApiError>
        {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(coderex_core::api_client::ApiError::Auth(
                "no credentials".into(),
            ))
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[coderex_core::tool_executor::ToolInfo],
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = coderex_core::api_client::StreamEvent> + Send + 'static>,
        > {
            Box::pin(futures::stream::empty())
        }
    }

    async fn spawn_and_wait(
        mgr: &mut AgentManager,
        prompt: &str,
        mut options: SpawnOptions,
    ) -> (String, AgentResult, tempfile::TempDir) {
        // Every `AgentManager` numbers its tasks from `task_0`, and the output
        // path is derived from that id. Tests run concurrently, so a shared
        // cwd would let one test read another test's output/meta file. Give
        // each spawn its own directory and return it so callers can still
        // inspect the artifacts.
        let cwd = tempfile::tempdir().unwrap();
        options.cwd = Some(cwd.path().to_path_buf());
        let instance = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                prompt,
                options,
                "sess_test",
                cwd.path().to_str().unwrap(),
                0,
            )
            .await
            .unwrap();
        let agent_id = instance.task_handle.id.clone();
        let result = mgr.wait_for_completion(&agent_id).await.unwrap();
        (agent_id, result, cwd)
    }

    /// The child engine must carry the background/agent deny hook. Its
    /// `agent_id` makes every permission request a background one, so a tool
    /// call must be denied with the hook's reason instead of reaching the
    /// (here absent) executor.
    #[tokio::test]
    async fn child_engine_denies_tool_calls_as_a_background_agent() {
        let mut mgr = make_manager();
        mgr.set_api_client(Box::new(ScriptedApi {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        let mut events = mgr.subscribe_all().expect("broadcast channel");

        let instance = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "read the note",
                SpawnOptions {
                    run_in_background: true,
                    ..SpawnOptions::default()
                },
                "sess_deny",
                "/tmp",
                0,
            )
            .await
            .unwrap();
        let agent_id = instance.task_handle.id.clone();
        mgr.wait_for_completion(&agent_id).await.unwrap();

        let mut denial = None;
        while let Ok(observed) = events.try_recv() {
            if let EngineEvent::ToolCallEnd { result, .. } = observed.event {
                if result.is_error {
                    denial = Some(result.output);
                }
            }
        }

        let denial = denial.expect("the denied tool call was never reported");
        assert!(
            denial.contains("denied in background agent mode"),
            "expected the background-agent denial, got: {denial}"
        );
    }

    /// The child request must carry the parent's prefix, not just store it on
    /// the instance. `fork_messages` is the definition of that request: prefix
    /// verbatim, then the task.
    #[tokio::test]
    async fn child_request_carries_the_parent_prefix() {
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mgr = make_manager();
        mgr.set_api_client(Box::new(CapturingApi {
            requests: Arc::clone(&requests),
        }));

        let cwd = tempfile::tempdir().unwrap();
        let prefix = parent_prefix();
        let options = SpawnOptions {
            parent_prefix: prefix.clone(),
            parent_cache_safe_params: Some(CacheSafeParams::new(
                "claude-test",
                "parent system prompt",
                vec!["Read".into()],
            )),
            ..Default::default()
        };

        let instance = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "sub-task",
                options,
                "sess_prefix",
                cwd.path().to_str().unwrap(),
                prefix.len(),
            )
            .await
            .unwrap();
        let agent_id = instance.task_handle.id.clone();
        let context = instance.context.clone().expect("forked context");
        let output_file = instance.output_file.clone().expect("output file");
        mgr.wait_for_completion(&agent_id).await.unwrap();

        let requests = requests.lock().unwrap();
        let first = requests.first().expect("the child never sent a request");
        assert_eq!(
            first,
            &context.fork_messages("sub-task"),
            "the child's first request did not start with the parent prefix"
        );

        // The fork's cache metadata is written for the production path, not
        // just computed in tests. The child's own system prompt differs from
        // the parent's, so the reuse is a conversation fork, not a cache hit.
        let meta_path = output_file.with_extension("meta.json");
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(meta_path).unwrap()).unwrap();
        assert_eq!(meta["fork"]["reused"], true);
        assert_eq!(meta["fork"]["prefix_messages"], prefix.len());
        assert_eq!(
            meta["fork"]["prefix_fingerprint"],
            context.prefix_fingerprint()
        );
        assert_eq!(meta["fork"]["cache_safe"], false);
    }

    /// `on_failure = retry` re-runs the attempt up to `max_retries`.
    #[tokio::test]
    async fn retry_policy_retry_reruns_until_max_retries() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut mgr = make_manager();
        mgr.set_api_client(Box::new(AlwaysFailApi {
            calls: Arc::clone(&calls),
        }));

        let (_agent_id, result, _cwd) = spawn_and_wait(
            &mut mgr,
            "do work",
            SpawnOptions {
                retry_policy: Some(RetryPolicy {
                    max_retries: 2,
                    on_failure: FailureAction::Retry,
                    escalation_prompt: Some("try again".into()),
                }),
                ..Default::default()
            },
        )
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(result.status, AgentStatus::Failed);
        assert!(
            result.output.contains("[retries exhausted after 2]"),
            "missing the retry-exhausted marker: {}",
            result.output
        );
    }

    /// `on_failure = escalate_to_parent` stops at the first failure, even when
    /// `max_retries` is non-zero, and marks the result as escalated.
    #[tokio::test]
    async fn retry_policy_escalate_stops_and_marks_the_result() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut mgr = make_manager();
        mgr.set_api_client(Box::new(AlwaysFailApi {
            calls: Arc::clone(&calls),
        }));

        let (agent_id, result, _cwd) = spawn_and_wait(
            &mut mgr,
            "do work",
            SpawnOptions {
                retry_policy: Some(RetryPolicy {
                    max_retries: 5,
                    on_failure: FailureAction::EscalateToParent,
                    escalation_prompt: None,
                }),
                ..Default::default()
            },
        )
        .await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "escalation must not retry"
        );
        assert_eq!(result.status, AgentStatus::Failed);
        assert!(
            result.output.contains("[escalated to parent]"),
            "missing the escalation marker: {}",
            result.output
        );

        let output_file = mgr
            .get_agent(&agent_id)
            .and_then(|agent| agent.output_file.clone())
            .expect("output file");
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(output_file.with_extension("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["escalated"], true);
    }

    /// A normally completing agent (the production task path, not
    /// `complete_agent`) must notify the manager's teammate callback.
    #[tokio::test]
    async fn completing_agent_invokes_the_teammate_callback() {
        let mut mgr = make_manager();
        mgr.set_api_client(Box::new(ScriptedApi {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));

        let seen: Arc<std::sync::Mutex<Vec<TeammateEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        mgr.set_event_callback(move |event| sink.lock().unwrap().push(event));

        let (_agent_id, result, _cwd) =
            spawn_and_wait(&mut mgr, "read the note", SpawnOptions::default()).await;
        assert_eq!(result.status, AgentStatus::Completed);

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|event| matches!(event, TeammateEvent::Idle { .. })),
            "no Idle teammate event: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|event| matches!(event, TeammateEvent::TaskCompleted { .. })),
            "no TaskCompleted teammate event: {seen:?}"
        );
    }

    #[tokio::test]
    async fn lifecycle_emits_tool_call_text_delta_and_idle() {
        let mut mgr = make_manager();
        mgr.set_api_client(Box::new(ScriptedApi {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.set_event_sender(tx);

        let instance = mgr
            .spawn_agent(
                AgentType::GeneralPurpose,
                "read the note",
                SpawnOptions::default(),
                "sess_lifecycle",
                "/tmp",
                0,
            )
            .await
            .unwrap();
        let agent_id = instance.task_handle.id.clone();
        mgr.wait_for_completion(&agent_id).await.unwrap();

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::AgentEvent { event, .. } = event {
                events.push(event);
            }
        }

        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentLifecycleEvent::Started)),
            "missing Started: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentLifecycleEvent::ToolCall { name, .. } if name == "Read"
            )),
            "missing ToolCall: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentLifecycleEvent::TextDelta(text) if text.contains("done from the mock")
            )),
            "missing TextDelta: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentLifecycleEvent::Idle { .. })),
            "missing Idle: {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentLifecycleEvent::Completed { summary } if summary.contains("done from the mock")
            )),
            "missing Completed: {events:?}"
        );
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
}
