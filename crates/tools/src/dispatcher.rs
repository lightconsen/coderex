//! Adapter that implements `coderex_core::tool_executor::ToolExecutor` by
//! dispatching through the in-crate `ToolRegistry` and `ToolExecutor`.
//!
//! This is the wiring point between the engine's abstract
//! `ToolExecutor` trait (in `coderex-core`) and the concrete registry
//! and execution pipeline in `coderex-tools`, so the engine can run
//! real tools without pulling `coderex-tools` into its dependency
//! graph.

use crate::context::{ExecutionContext, ToolError};
use crate::executor::ToolExecutor as InnerExecutor;
use crate::registry::ToolRegistry;
use crate::tool::{ToolContext, ToolInput};
use coderex_core::tool_executor::{
    ToolDispatchOutcome, ToolExecutor as CoreToolExecutor,
};
use coderex_core::types::ToolResult;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// A `ToolExecutor` (core trait) implementation backed by the in-crate
/// `ToolRegistry` + `ToolExecutor` pipeline.
#[allow(missing_debug_implementations)]
pub struct RegistryToolDispatcher {
    registry: Arc<ToolRegistry>,
    executor: Arc<InnerExecutor>,
    context: ExecutionContext,
    cwd: PathBuf,
}

impl RegistryToolDispatcher {
    /// Construct a new dispatcher for the given registry and execution
    /// environment.
    #[must_use]
    pub fn new(
        registry: Arc<ToolRegistry>,
        executor: Arc<InnerExecutor>,
        context: ExecutionContext,
        cwd: PathBuf,
    ) -> Self {
        Self {
            registry,
            executor,
            context,
            cwd,
        }
    }
}

#[async_trait::async_trait]
impl CoreToolExecutor for RegistryToolDispatcher {
    async fn execute(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> ToolDispatchOutcome {
        let tool = match self.registry.get_tool_arc(tool_name) {
            Some(tool) => tool,
            None => {
                return ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output: format!(
                            "tool '{tool_name}' not found in registry"
                        ),
                        is_error: true,
                    },
                };
            }
        };

        let ctx = ToolContext::new(self.context, &self.cwd);
        let call_input = ToolInput {
            call_id: tool_call_id.to_string(),
            name: tool_name.to_string(),
            arguments: input.clone(),
        };

        match self
            .executor
            .execute_single(tool.as_ref(), call_input, ctx)
            .await
        {
            Ok(output) => ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: output.output,
                    is_error: false,
                },
            },
            Err(err) => {
                let message = match &err {
                    ToolError::NotFound { name } => format!("tool '{name}' not found"),
                    ToolError::Timeout { timeout } => {
                        format!("tool '{tool_name}' timed out after {timeout:?}")
                    }
                    ToolError::Io(e) => format!("tool '{tool_name}' io error: {e}"),
                    ToolError::InvalidInput { message } => {
                        format!("tool '{tool_name}' invalid input: {message}")
                    }
                    ToolError::PermissionDenied { name } => {
                        format!("tool '{name}' permission denied")
                    }
                };
                ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output: message,
                        is_error: true,
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::EchoTool;
    use crate::executor::ExecutionConfig;

    fn registry_with_echo() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        Arc::new(registry)
    }

    #[tokio::test]
    async fn dispatches_to_real_tool() {
        let registry = registry_with_echo();
        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );
        let outcome = dispatcher
            .execute("id-1", "echo", &serde_json::json!({"message": "hi"}))
            .await;
        assert!(!outcome.result.is_error);
        assert_eq!(outcome.result.tool_call_id, "id-1");
        assert_eq!(outcome.result.output, "hi");
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let registry = Arc::new(ToolRegistry::new());
        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );
        let outcome = dispatcher
            .execute("id-x", "no_such_tool", &serde_json::Value::Null)
            .await;
        assert!(outcome.result.is_error);
        assert!(outcome.result.output.contains("no_such_tool"));
        assert!(outcome.result.output.contains("not found"));
    }
}
