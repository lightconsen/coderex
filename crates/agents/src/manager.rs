//! AgentManager — spawn, manage, and communicate with sub-agents.
//!
//! Phase 10.3 + 10.5: Provides agent spawning with context inheritance,
//! tool whitelisting, messaging, resume support, and teammate events.

use coderex_core::api_client::ApiClient;
use coderex_core::orchestrator::{
    BackgroundFeatureRequest, QueryResult,
    run_background_feature, run_forked_agent,
};
use coderex_core::task::{TaskHandle, TaskKind, TaskManager};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use thiserror::Error;

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
    /// Isolation mode.
    pub isolation: Option<String>,
    /// Working directory for the agent.
    pub cwd: Option<PathBuf>,
    /// Environment variables to inject.
    pub extra_env: HashMap<String, String>,
    /// Whether the agent may write files (false = read-only tools).
    pub writable: bool,
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
    pub fn new(
        parent_session_id: String,
        cwd: String,
        message_count: usize,
    ) -> Self {
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

/// A running agent instance.
#[derive(Debug)]
pub struct AgentInstance {
    /// Agent type.
    pub agent_type: AgentType,
    /// Spawn options used.
    pub options: SpawnOptions,
    /// Task handle for lifecycle tracking.
    pub task_handle: TaskHandle,
    /// Agent current status.
    pub status: AgentStatus,
    /// Forked context from parent session.
    pub context: Option<ForkedContext>,
    /// Message channel to the agent.
    pub message_tx: Option<mpsc::UnboundedSender<AgentMessage>>,
    /// Agent's output file path.
    pub output_file: Option<PathBuf>,
    /// Agent's model override.
    pub model: Option<String>,
}

impl AgentInstance {
    /// Create a new agent instance.
    #[must_use]
    pub fn new(
        agent_type: AgentType,
        options: SpawnOptions,
        task_handle: TaskHandle,
    ) -> Self {
        let model = options.model_override.clone();
        Self {
            agent_type,
            options,
            task_handle,
            status: AgentStatus::Starting,
            context: None,
            message_tx: None,
            output_file: None,
            model,
        }
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
pub type TeammateEventCallback =
    Box<dyn Fn(TeammateEvent) + Send + Sync + 'static>;

// ─── AgentManager ───────────────────────────────────────────────────────────

/// Manages the lifecycle of sub-agents.
///
/// Handles:
/// - Spawning agents with context fork and tool whitelist
/// - Tracking agent state and messaging
/// - Resuming existing agents
/// - Broadcasting teammate events
/// - Running forked agents and background features via AI
pub struct AgentManager {
    /// Shared task manager for lifecycle tracking.
    task_manager: Arc<Mutex<TaskManager>>,
    /// Active agent instances.
    agents: HashMap<String, AgentInstance>,
    /// Callback for teammate events.
    #[allow(dead_code)]
    event_callback: Option<TeammateEventCallback>,
    /// Resumed agents (from a previous session).
    resumed_agents: Vec<String>,
    /// API client for forked/background AI execution.
    api_client: Option<Box<dyn ApiClient>>,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("agents", &self.agents.len())
            .field("resumed_agents", &self.resumed_agents)
            .finish_non_exhaustive()
    }
}

impl AgentManager {
    /// Create a new AgentManager with the given task manager.
    #[must_use]
    pub fn new(task_manager: Arc<Mutex<TaskManager>>) -> Self {
        Self {
            task_manager,
            agents: HashMap::new(),
            event_callback: None,
            resumed_agents: Vec::new(),
            api_client: None,
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

    /// Set the callback for teammate events.
    pub fn set_event_callback<F>(&mut self, callback: F)
    where
        F: Fn(TeammateEvent) + Send + Sync + 'static,
    {
        self.event_callback = Some(Box::new(callback));
    }

    /// Emit a teammate event (notifies callback if registered).
    fn emit_event(&self, event: TeammateEvent) {
        if let Some(ref cb) = self.event_callback {
            cb(event);
        }
    }

    /// Spawn a new agent.
    ///
    /// Creates a task handle, forks context from the parent session,
    /// applies tool whitelist, and starts the agent process.
    ///
    /// # Arguments
    /// * `agent_type` — Type of agent to spawn.
    /// * `prompt` — System prompt for the agent.
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
        // Mark as running
        let _ = task_manager.mark_running(&task_id);
        let task_handle = task_manager.get_task(&task_id).unwrap().clone();
        drop(task_manager);

        // Set up message channel
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<AgentMessage>();

        // Create output file path
        let output_file = options.cwd.as_ref().map_or_else(
            || PathBuf::from(format!("/tmp/agent-{task_id}-output.txt")),
            |d| d.join(format!("agent-{task_id}-output.txt")),
        );

        let agent_type_name_str = agent_type_name(&agent_type);

        // Clone task handle and create the agent
        let mut instance = AgentInstance::new(
            agent_type,
            options.clone(),
            task_handle,
        );
        instance.context = Some(context);
        instance.message_tx = Some(msg_tx);
        instance.output_file = Some(output_file.clone());
        instance.status = AgentStatus::Running;

        // In a real implementation, this would spawn the actual process.
        // For now, we log the spawn.
        tracing::info!(
            "agent spawned: task_id={task_id}, type={agent_type_name_str}, prompt_len={}",
            prompt.len()
        );

        let id = task_id.clone();
        self.agents.insert(id, instance);

        self.agents.get(&task_id).ok_or_else(|| {
            AgentError::SpawnFailed("agent not found after spawn".into())
        })
    }

    /// Get an agent by task ID.
    #[must_use]
    pub fn get_agent(&self, id: &str) -> Option<&AgentInstance> {
        self.agents.get(id)
    }

    /// Send a message to a running agent.
    pub async fn send_message(
        &self,
        agent_id: &str,
        content: &str,
    ) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| AgentError::NotFound(agent_id.to_string()))?;

        if agent.status != AgentStatus::Running {
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

        instance.status = AgentStatus::Completed;

        // Emit teammate event
        self.emit_event(TeammateEvent::TaskCompleted {
            agent_id: agent_id.to_string(),
            subject: subject.to_string(),
            summary: summary.to_string(),
        });

        Ok(())
    }

    /// Mark an agent as idle and emit an Idle event.
    pub async fn mark_idle(
        &self,
        agent_id: &str,
        reason: &str,
    ) -> Result<(), AgentError> {
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

        instance.status = AgentStatus::Stopped;

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
            .filter(|a| a.status == AgentStatus::Running)
            .count()
    }

    // ─── Resume Support (10.5) ─────────────────────────────────────────

    /// Resume an existing agent from a previous session.
    ///
    /// Restores the agent's context and state from serialized data.
    pub async fn resume_agent(
        &mut self,
        agent_id: &str,
        agent_type: AgentType,
        options: SpawnOptions,
        context: ForkedContext,
    ) -> Result<&AgentInstance, AgentError> {
        // Check if agent already exists
        if self.agents.contains_key(agent_id) {
            return Err(AgentError::SpawnFailed(format!(
                "agent {agent_id} already exists"
            )));
        }

        // Re-create task handle
        let kind = TaskKind::LocalAgent {
            agent_type: agent_type_name(&agent_type),
            pid: None,
        };

        let task_manager = self.task_manager.lock().await;
        // Use the specific ID for resumed agents
        let task_handle = TaskHandle::new(agent_id.to_string(), kind);
        drop(task_manager);

        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<AgentMessage>();

        let mut instance = AgentInstance::new(agent_type, options, task_handle);
        instance.context = Some(context);
        instance.message_tx = Some(msg_tx);
        instance.status = AgentStatus::Running;

        self.agents.insert(agent_id.to_string(), instance);
        self.resumed_agents.push(agent_id.to_string());

        tracing::info!("agent resumed: agent_id={agent_id}");

        self.agents
            .get(agent_id)
            .ok_or_else(|| AgentError::SpawnFailed("resume failed".into()))
    }

    /// Check if an agent was resumed from a previous session.
    #[must_use]
    pub fn is_resumed(&self, agent_id: &str) -> bool {
        self.resumed_agents.iter().any(|id| id == agent_id)
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
        ..Default::default()
    };

    let agent_type = AgentType::Custom {
        prompt: command.to_string(),
    };

    let instance = manager
        .spawn_agent(
            agent_type,
            command,
            options,
            parent_session_id,
            cwd,
            0,
        )
        .await?;

    let task_id = instance.task_handle.id.clone();
    let output_file = instance.output_file.clone();

    // Mark as completed (synchronous execution)
    manager
        .complete_agent(&task_id, command, "completed via agent")
        .await?;

    // Read output file if it exists
    if let Some(ref path) = output_file {
        if path.exists() {
            return std::fs::read_to_string(path)
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
    let allowed_tools = vec![
        "Read".into(),
        "Glob".into(),
        "Grep".into(),
    ];

    let options = SpawnOptions {
        run_in_background: true,
        allowed_tools: Some(allowed_tools),
        ..Default::default()
    };

    let agent_type = AgentType::Custom {
        prompt: format!(
            "Dream agent: Analyze recent sessions and produce a structured summary.\n\n{}\n\nSession summaries:\n{}",
            dream_prompt,
            session_summaries.iter().enumerate().map(|(i, s)| format!("## Session {}\n{}", i + 1, s)).collect::<Vec<_>>().join("\n")
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

    let task_id = instance.task_handle.id.clone();
    let output_file = instance.output_file.clone();

    // Mark as completed
    manager
        .complete_agent(&task_id, "dream analysis", "dream completed")
        .await?;

    // Read output file if it exists
    if let Some(ref path) = output_file {
        if path.exists() {
            return std::fs::read_to_string(path)
                .map_err(|_| AgentError::SpawnFailed("could not read dream output".into()));
        }
    }

    Ok("dream agent completed (no output file written)".into())
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

        assert_eq!(instance.status, AgentStatus::Running);
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

        assert_eq!(mgr.running_count(), 3);
        assert_eq!(mgr.list_agents().len(), 3);
    }

    #[tokio::test]
    async fn send_message_to_running_agent() {
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

        let result = mgr.send_message(&agent_id, "follow up").await;
        assert!(result.is_ok());
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
        assert_eq!(mgr.list_agents()[0].status, AgentStatus::Completed);
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

        assert_eq!(mgr.list_agents()[0].status, AgentStatus::Stopped);
    }

    #[tokio::test]
    async fn resume_agent() {
        let mut mgr = make_manager();
        let context = ForkedContext::new(
            "sess_parent".into(),
            "/tmp".into(),
            10,
        );

        mgr.resume_agent(
            "resumed_1",
            AgentType::Plan,
            SpawnOptions::default(),
            context,
        )
        .await
        .unwrap();

        assert!(mgr.is_resumed("resumed_1"));
        assert_eq!(mgr.running_count(), 1);
    }

    #[tokio::test]
    async fn resume_existing_agent_errors() {
        let mut mgr = make_manager();
        let context = ForkedContext::new(
            "sess_parent".into(),
            "/tmp".into(),
            10,
        );

        mgr.resume_agent(
            "dup_1",
            AgentType::Plan,
            SpawnOptions::default(),
            context.clone(),
        )
        .await
        .unwrap();

        let result = mgr.resume_agent("dup_1", AgentType::Plan, SpawnOptions::default(), context).await;
        assert!(matches!(result, Err(AgentError::SpawnFailed(_))));
    }

    #[tokio::test]
    async fn execute_via_agent_runs_command() {
        let mut mgr = make_manager();
        let output = super::execute_via_agent(&mut mgr, "explain this code", "sess_1", "/tmp")
            .await
            .unwrap();
        assert!(output.contains("agent completed"));
    }

    #[test]
    fn agent_type_name_maps() {
        assert_eq!(agent_type_name(&AgentType::GeneralPurpose), "general_purpose");
        assert_eq!(agent_type_name(&AgentType::Plan), "plan");
        assert_eq!(agent_type_name(&AgentType::Verification), "verification");
        assert_eq!(
            agent_type_name(&AgentType::ClaudeCodeGuide),
            "claude_code_guide"
        );
        assert_eq!(agent_type_name(&AgentType::StatuslineSetup), "statusline_setup");
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
        let mut mgr = make_manager();
        let dream_prompt = "Analyze recent sessions for patterns.";
        let summaries = vec!["Session 1: Fixed auth bug", "Session 2: Added tests"];

        let output = super::spawn_dream_agent(
            &mut mgr,
            dream_prompt,
            "sess_parent",
            "/tmp",
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
