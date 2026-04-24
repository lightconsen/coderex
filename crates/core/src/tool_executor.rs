//! Tool executor trait — abstract tool dispatch for `QueryEngine`.
//!
//! This trait lives in `coderex-core` to avoid a circular dependency with
//! `coderex-tools`. Concrete implementations (backed by `ToolRegistry`)
//! live in `coderex-tools`.
//!
//! The engine delegates tool dispatch through this trait rather than
//! embedding registry knowledge directly, so test code can substitute a
//! simple in-memory executor and production code can wire the full
//! registry from `coderex-tools`.

use crate::types::ToolResult;
use serde_json::Value;

/// Outcome of dispatching a single tool call through a `ToolExecutor`.
#[derive(Debug, Clone)]
pub struct ToolDispatchOutcome {
    /// The tool result to feed back to the model.
    pub result: ToolResult,
}

/// Metadata for a tool that can be advertised to the LLM.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    /// Unique tool name.
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON schema describing the tool's input parameters.
    pub input_schema: Value,
}

/// Abstract tool dispatcher — implemented by `coderex-tools::RegistryToolExecutor`.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// List available tools with metadata for LLM advertisement.
    fn list_tools(&self) -> Vec<ToolInfo> {
        Vec::new()
    }

    /// Execute a tool by name with the given input and return its result.
    ///
    /// Implementations should return a `ToolResult` with `is_error = true`
    /// (and a descriptive `output` message) when the tool is unknown or
    /// fails — they must NEVER panic.
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> ToolDispatchOutcome;
}

/// No-op tool executor — used when no concrete tool registry is wired.
///
/// Returns a proper error `ToolResult` instead of a placeholder success
/// string so the model sees the tool as genuinely unavailable.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpToolExecutor;

#[async_trait::async_trait]
impl ToolExecutor for NoOpToolExecutor {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        _input: &Value,
    ) -> ToolDispatchOutcome {
        ToolDispatchOutcome {
            result: ToolResult {
                tool_call_id: tool_call_id.to_string(),
                output: format!(
                    "tool '{tool_name}' is not registered in this engine; no tool executor was configured"
                ),
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_returns_error_result() {
        let exec = NoOpToolExecutor;
        let outcome = exec
            .execute("id-1", "read_file", &Value::Null)
            .await;
        assert!(outcome.result.is_error);
        assert_eq!(outcome.result.tool_call_id, "id-1");
        assert!(outcome.result.output.contains("not registered"));
        assert!(outcome.result.output.contains("read_file"));
    }
}
