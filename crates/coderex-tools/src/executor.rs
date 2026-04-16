//! Tool executor — dispatches tool calls with concurrency control and timeout.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolInput};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

/// Configuration for the tool executor.
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    /// Default timeout for tool execution.
    pub default_timeout: Duration,
    /// Maximum number of concurrent tool executions.
    pub max_concurrent: usize,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(60),
            max_concurrent: 4,
        }
    }
}

/// Executes tools with concurrency control and timeout.
#[derive(Debug)]
pub struct ToolExecutor {
    config: ExecutionConfig,
    /// Per-tool-name locks for non-concurrency-safe tools.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl ToolExecutor {
    /// Create a new executor with the given config.
    #[must_use]
    pub fn new(config: ExecutionConfig) -> Self {
        Self {
            config,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Get or create a lock for a specific tool name.
    async fn get_lock(&self, tool_name: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(tool_name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Execute a single tool call.
    ///
    /// Respects the tool's concurrency safety flag:
    /// - Concurrency-safe tools may run in parallel
    /// - Non-safe tools are serialized per-tool-name
    pub async fn execute_single(
        &self,
        tool: &dyn Tool,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let desc = tool.descriptor();

        // For non-concurrency-safe tools, serialize execution
        if !desc.is_concurrency_safe {
            let tool_name = desc.name.clone();
            let lock = self.get_lock(&tool_name).await;
            let _guard = lock.lock().await;
            return timeout(self.config.default_timeout, tool.execute(input, ctx))
                .await
                .map_err(|_| ToolError::Timeout {
                    timeout: self.config.default_timeout,
                })?;
        }

        // Concurrency-safe tools run without serialization
        timeout(self.config.default_timeout, tool.execute(input, ctx))
            .await
            .map_err(|_| ToolError::Timeout {
                timeout: self.config.default_timeout,
            })?
    }

    /// Execute multiple tool calls in concurrent batches.
    ///
    /// Tools are batched according to `max_concurrent`. Each tool in the
    /// batch runs concurrently unless it is marked non-concurrency-safe.
    pub async fn execute_many(
        &self,
        calls: Vec<(Arc<dyn Tool>, ToolInput, ToolContext<'static>)>,
    ) -> Vec<Result<ToolOutput, ToolError>> {
        let mut all_results = Vec::with_capacity(calls.len());

        for batch in calls.chunks(self.config.max_concurrent) {
            let mut handles = Vec::new();

            for (tool, input, ctx) in batch {
                let tool = Arc::clone(tool);
                let input = input.clone();
                let ctx = ctx.clone();
                let exec_timeout = self.config.default_timeout;

                let handle = tokio::spawn(async move {
                    let desc = tool.descriptor();

                    drop(desc);

                    tokio::time::timeout(exec_timeout, tool.execute(input, ctx))
                        .await
                        .map_err(|_| ToolError::Timeout { timeout: exec_timeout })?
                });

                handles.push(handle);
            }

            for handle in handles {
                match handle.await {
                    Ok(result) => all_results.push(result),
                    Err(_) => all_results.push(Err(ToolError::InvalidInput {
                        message: "task panicked".into(),
                    })),
                }
            }
        }

        all_results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{EchoTool, NoOpTool};
    use crate::tool::ToolContext;
    use crate::tool::ToolInput;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(
            crate::context::ExecutionContext::Normal,
            Path::new("/tmp"),
        )
    }

    fn test_input(msg: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "echo".into(),
            arguments: serde_json::json!({"message": msg}),
        }
    }

    #[tokio::test]
    async fn execute_single_echo_tool() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let result = executor
            .execute_single(&EchoTool, test_input("hello"), test_ctx())
            .await
            .unwrap();
        assert_eq!(result.output, "hello");
    }

    #[tokio::test]
    async fn execute_single_noop_tool() {
        let executor = ToolExecutor::new(ExecutionConfig::default());
        let result = executor
            .execute_single(&NoOpTool, test_input(""), test_ctx())
            .await
            .unwrap();
        assert!(result.output.is_empty());
    }

    #[tokio::test]
    async fn execute_many_runs_tools_concurrently() {
        let executor = ToolExecutor::new(ExecutionConfig::default());

        let calls: Vec<(Arc<dyn Tool>, ToolInput, ToolContext<'static>)> = vec![
            (Arc::new(EchoTool), test_input("a"), test_ctx()),
            (Arc::new(EchoTool), test_input("b"), test_ctx()),
            (Arc::new(NoOpTool), test_input(""), test_ctx()),
        ];

        let results = executor.execute_many(calls).await;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap().output, "a");
        assert_eq!(results[1].as_ref().unwrap().output, "b");
        assert!(results[2].as_ref().unwrap().output.is_empty());
    }
}
