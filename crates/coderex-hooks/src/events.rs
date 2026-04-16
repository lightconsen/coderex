//! Hook event definitions and result types.

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
}

/// What the hook decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Continue with the operation.
    Continue,
    /// Stop the operation.
    Stop,
}
