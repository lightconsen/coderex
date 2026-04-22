//! Tool execution context, error types, and output.

use thiserror::Error;

/// Execution context that filters which tools are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionContext {
    /// Standard interactive session.
    Normal,
    /// Async agent running in background.
    AsyncAgent,
    /// Coordinator managing multiple sub-agents.
    CoordinatorMode,
    /// Multi-agent team in a single process.
    InProcessTeam,
    /// Plan mode: read-only, no mutations.
    PlanMode,
    /// REPL-only: limited to command interface.
    ReplOnly,
    /// Headless SDK integration.
    HeadlessSdk,
    /// Remote bridge to external tool server.
    RemoteBridge,
}

/// Errors that can occur during tool execution.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The requested tool was not found.
    #[error("tool '{name}' not found")]
    NotFound {
        /// The name of the tool that was not found.
        name: String,
    },
    /// Tool execution timed out.
    #[error("execution timeout after {timeout:?}")]
    Timeout {
        /// The timeout duration that elapsed.
        timeout: std::time::Duration,
    },
    /// I/O error during tool execution.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Tool input was invalid.
    #[error("invalid input: {message}")]
    InvalidInput {
        /// The error message describing the invalid input.
        message: String,
    },
    /// Permission denied for tool execution.
    #[error("permission denied for tool '{name}'")]
    PermissionDenied {
        /// The name of the tool that was denied.
        name: String,
    },
    /// Rate limit exceeded for tool execution.
    #[error("rate limit exceeded for tool '{name}'")]
    RateLimited {
        /// The name of the tool that was rate limited.
        name: String,
    },
    /// User input required but elicitation channel unavailable.
    #[error("user input required but elicitation channel unavailable")]
    ElicitationUnavailable,
}

/// Output from a successful tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text output from the tool.
    pub output: String,
    /// Whether this output should be displayed to the user.
    pub is_display: bool,
}
