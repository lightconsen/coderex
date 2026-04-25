//! Tool executor — dispatches tool calls with concurrency control and timeout.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolInput};
use crate::tool_cache::{ToolCache, ToolCacheConfig};
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
    /// Whether to enforce sandbox restrictions for tool execution.
    pub sandbox_enabled: bool,
    /// Maximum output size in bytes before truncation.
    pub max_output_bytes: usize,
    /// Cache configuration for tool results.
    pub cache_config: ToolCacheConfig,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(60),
            max_concurrent: 4,
            sandbox_enabled: true,
            max_output_bytes: 1_048_576, // 1 MB
            cache_config: ToolCacheConfig::default(),
        }
    }
}

/// Executes tools with concurrency control, timeout, and result caching.
#[derive(Debug)]
pub struct ToolExecutor {
    config: ExecutionConfig,
    /// Per-tool-name locks for non-concurrency-safe tools.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Optional global result cache for read-only tools.
    cache: Option<ToolCache>,
}

impl ToolExecutor {
    /// Create a new executor with the given config.
    #[must_use]
    pub fn new(config: ExecutionConfig) -> Self {
        let cache = if config.cache_config.enabled {
            Some(ToolCache::new(config.cache_config.clone()))
        } else {
            None
        };
        Self {
            config,
            locks: Mutex::new(HashMap::new()),
            cache,
        }
    }

    /// Return whether sandboxing is enabled.
    #[must_use]
    pub const fn sandbox_enabled(&self) -> bool {
        self.config.sandbox_enabled
    }

    /// Truncate tool output if it exceeds `max_output_bytes`.
    fn truncate_output(&self, mut output: ToolOutput) -> ToolOutput {
        let max = self.config.max_output_bytes;
        if output.output.len() > max {
            let mut truncated = output.output;
            truncated.truncate(max);
            truncated.push_str("\n... [truncated]");
            output.output = truncated;
        }
        output
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
    ///
    /// Read-only results are cached and served from cache when possible.
    pub async fn execute_single(
        &self,
        tool: &dyn Tool,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let desc = tool.descriptor();

        // Try cache for read-only tools
        if desc.is_read_only {
            if let Some(ref cache) = self.cache {
                if let Some(cached) = cache.get(&desc.name, &input.arguments).await {
                    return Ok(self.truncate_output(cached));
                }
            }
        }

        // For non-concurrency-safe tools, serialize execution
        let result = if !desc.is_concurrency_safe {
            let tool_name = desc.name.clone();
            let lock = self.get_lock(&tool_name).await;
            let _guard = lock.lock().await;
            timeout(
                self.config.default_timeout,
                tool.execute(input.clone(), ctx),
            )
            .await
            .map_err(|_| ToolError::Timeout {
                timeout: self.config.default_timeout,
            })?
        } else {
            // Concurrency-safe tools run without serialization
            timeout(
                self.config.default_timeout,
                tool.execute(input.clone(), ctx),
            )
            .await
            .map_err(|_| ToolError::Timeout {
                timeout: self.config.default_timeout,
            })?
        };

        let result = result.map(|output| self.truncate_output(output));

        // Cache successful read-only results
        if desc.is_read_only {
            if let (Some(cache), Ok(output)) = (&self.cache, &result) {
                cache
                    .insert(&desc.name, &input.arguments, output.clone())
                    .await;
            }
        }

        result
    }

    /// Execute multiple tool calls in concurrent batches.
    ///
    /// Tools are batched according to `max_concurrent`. Each tool in the
    /// batch runs concurrently unless it is marked non-concurrency-safe.
    ///
    /// Read-only results are cached and may be served from cache without
    /// spawning a task.
    pub async fn execute_many(
        &self,
        calls: Vec<(Arc<dyn Tool>, ToolInput, ToolContext<'static>)>,
    ) -> Vec<Result<ToolOutput, ToolError>> {
        let mut all_results: Vec<Option<Result<ToolOutput, ToolError>>> =
            std::iter::repeat_with(|| None).take(calls.len()).collect();
        let mut to_spawn: Vec<(usize, Arc<dyn Tool>, ToolInput, ToolContext<'static>)> = Vec::new();

        // Phase 1: check cache
        for (idx, (tool, input, ctx)) in calls.into_iter().enumerate() {
            let desc = tool.descriptor();
            if desc.is_read_only {
                if let Some(ref cache) = self.cache {
                    if let Some(cached) = cache.get(&desc.name, &input.arguments).await {
                        all_results[idx] = Some(Ok(self.truncate_output(cached)));
                        continue;
                    }
                }
            }
            to_spawn.push((idx, tool, input, ctx));
        }

        // Phase 2: execute remaining calls in batches
        for batch in to_spawn.chunks(self.config.max_concurrent) {
            let mut handles = Vec::new();

            for (idx, tool, input, ctx) in batch {
                let idx = *idx;
                let tool = Arc::clone(tool);
                let input = input.clone();
                let ctx = ctx.clone();
                let exec_timeout = self.config.default_timeout;
                let max_output_bytes = self.config.max_output_bytes;
                let cache = self.cache.clone();

                let handle = tokio::spawn(async move {
                    let desc = tool.descriptor();
                    let is_read_only = desc.is_read_only;
                    let tool_name = desc.name.clone();

                    let result =
                        match tokio::time::timeout(exec_timeout, tool.execute(input.clone(), ctx))
                            .await
                        {
                            Ok(Ok(mut output)) => {
                                if output.output.len() > max_output_bytes {
                                    output.output.truncate(max_output_bytes);
                                    output.output.push_str("\n... [truncated]");
                                }
                                Ok(output)
                            }
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(ToolError::Timeout {
                                timeout: exec_timeout,
                            }),
                        };

                    // Cache successful read-only results
                    if is_read_only {
                        if let (Some(cache), Ok(output)) = (&cache, &result) {
                            cache
                                .insert(&tool_name, &input.arguments, output.clone())
                                .await;
                        }
                    }

                    (idx, result)
                });

                handles.push(handle);
            }

            for handle in handles {
                match handle.await {
                    Ok((idx, result)) => {
                        all_results[idx] = Some(result);
                    }
                    Err(_) => {
                        // Task panicked
                    }
                }
            }
        }

        all_results
            .into_iter()
            .map(|r| {
                r.unwrap_or(Err(ToolError::InvalidInput {
                    message: "internal batch error".into(),
                }))
            })
            .collect()
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
        ToolContext::new(crate::context::ExecutionContext::Normal, Path::new("/tmp"))
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
