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
    /// The tool ran but the work it delegated to failed.
    ///
    /// Used by tools that hand the call to something outside the process
    /// (an MCP server, a remote endpoint) and get a failure back.
    #[error("tool '{name}' failed: {message}")]
    Execution {
        /// The name of the tool that failed.
        name: String,
        /// The failure message.
        message: String,
    },
}

/// Output from a successful tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text output from the tool.
    pub output: String,
    /// Whether this output should be displayed to the user.
    pub is_display: bool,
    /// Images the tool returned, forwarded to the model as image blocks.
    ///
    /// Most tools leave this empty; MCP image content and an image read use
    /// it so the model sees the image instead of base64 text.
    pub images: Vec<coderex_core::types::ImageBlock>,
}

impl ToolOutput {
    /// Text-only output.
    #[must_use]
    pub fn text(output: impl Into<String>, is_display: bool) -> Self {
        Self {
            output: output.into(),
            is_display,
            images: Vec::new(),
        }
    }
}
