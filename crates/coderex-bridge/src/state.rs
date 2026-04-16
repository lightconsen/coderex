//! Bridge session state machine.

/// State of a bridged session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeSessionState {
    /// Session has been assigned to a worker.
    Assigned,
    /// Worker is preparing the environment.
    Preparing,
    /// Creating a git worktree for the session.
    CreatingWorktree,
    /// Spawning the CodeRex process.
    Spawning,
    /// Session is actively running.
    Running,
    /// Waiting for user permission on a tool call.
    AwaitingPermission,
    /// Archiving session results.
    Archiving,
    /// Cleaning up session resources.
    CleaningUp,
    /// Session completed successfully.
    Completed,
    /// Session failed with an error message.
    Failed(String),
}
