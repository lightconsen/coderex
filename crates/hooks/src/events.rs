//! Hook event definitions and result types.

use serde::{Deserialize, Serialize};

/// All 22 hook events that can be triggered during the CodeRex lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum HookEvent {
    /// Before a tool is used.
    PreToolUse,
    /// After a tool is used.
    PostToolUse,
    /// After a tool use fails.
    PostToolUseFailure,
    /// Before a user prompt is submitted.
    UserPromptSubmit,
    /// When a session starts.
    SessionStart,
    /// During initial setup.
    Setup,
    /// When a sub-agent starts.
    SubagentStart,
    /// When a notification is emitted.
    Notification,
    /// When permission is requested.
    PermissionRequest,
    /// When permission is denied.
    PermissionDenied,
    /// When the working directory changes.
    CwdChanged,
    /// When a file changes.
    FileChanged,
    /// When a worktree is created.
    WorktreeCreate,
    /// Before context compaction.
    PreCompact,
    /// After context compaction.
    PostCompact,
    /// When the user requests a stop.
    Stop,
    /// When a sub-agent stops.
    SubagentStop,
    /// When a task is completed.
    TaskCompleted,
    /// When a teammate goes idle.
    TeammateIdle,
    /// When an elicitation is requested.
    Elicitation,
    /// When an elicitation result is received.
    ElicitationResult,
    /// At the end of a turn.
    TurnEnd,
}

/// The result of a hook execution.
#[derive(Debug, Clone)]
pub struct HookResult {
    /// Whether to continue or stop the operation.
    pub action: HookAction,
    /// Whether to suppress the normal output.
    pub suppress_output: bool,
    /// Updated input to replace the original input.
    pub updated_input: Option<String>,
    /// Additional context to append.
    pub additional_context: Option<String>,
    /// File paths to watch (for file watcher hooks).
    pub watch_paths: Vec<String>,
    /// Permission decision (for PermissionRequest hooks).
    pub permission_decision: Option<HookPermissionDecision>,
}

/// What the hook decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Continue with the operation.
    Continue,
    /// Stop the operation.
    Stop,
}

/// Permission decision from a hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPermissionDecision {
    /// Whether to allow the action.
    pub allow: bool,
    /// If allow is true, optionally restrict to a specific path.
    pub allow_for_path: Option<String>,
    /// Reason for the decision.
    pub reason: String,
}

/// Hook execution errors.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// Hook timed out.
    #[error("hook timed out after {0}ms")]
    Timeout(u64),
    /// Failed to spawn the hook process.
    #[error("failed to spawn hook process: {0}")]
    SpawnFailed(String),
    /// Configuration not found.
    #[error("hook configuration not found")]
    ConfigNotFound,
    /// Invalid command in hook configuration.
    #[error("invalid hook command: {0}")]
    InvalidCommand(String),
    /// JSON parse/serialize error.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    /// Hook execution failed (non-zero exit).
    #[error("hook exited with code {0}")]
    ExecutionFailed(i32),
}
