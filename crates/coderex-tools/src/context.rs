//! Tool execution context and error types.

use thiserror::Error;

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
}

/// Output from a successful tool execution.
#[derive(Debug)]
pub struct ToolOutput {
    /// Text output from the tool.
    pub output: String,
    /// Whether this output should be displayed to the user.
    pub is_display: bool,
}
