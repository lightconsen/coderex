//! Adapter that implements `coderex_core::tool_executor::ToolExecutor` by
//! dispatching through the in-crate `ToolRegistry` and `ToolExecutor`.
//!
//! This is the wiring point between the engine's abstract
//! `ToolExecutor` trait (in `coderex-core`) and the concrete registry
//! and execution pipeline in `coderex-tools`, so the engine can run
//! real tools without pulling `coderex-tools` into its dependency
//! graph.

use crate::builtins::chrome::{CHROME_CLIENT_KEY, ChromeClient};
use crate::context::{ExecutionContext, ToolError};
use crate::cron_scheduler::{CRON_SCHEDULER_KEY, CronScheduler};
use crate::executor::ToolExecutor as InnerExecutor;
use crate::rate_limiter::{RATE_LIMITER_KEY, RateLimiter};
use crate::registry::ToolRegistry;
use crate::sandbox_policy;
use crate::task_store::{TASK_STORE_KEY, TaskStore};
use crate::tool::{SharedState, ToolContext, ToolInput};
use coderex_core::tool_executor::{ToolDispatchOutcome, ToolExecutor as CoreToolExecutor};
use coderex_core::types::ToolResult;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A `ToolExecutor` (core trait) implementation backed by the in-crate
/// `ToolRegistry` + `ToolExecutor` pipeline.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct RegistryToolDispatcher {
    registry: Arc<ToolRegistry>,
    executor: Arc<InnerExecutor>,
    context: ExecutionContext,
    cwd: PathBuf,
    sandbox_enabled: bool,
    sandbox_override: Option<coderex_core::sandbox::SandboxConfig>,
    task_store: Option<Arc<TaskStore>>,
    cron_scheduler: Option<Arc<CronScheduler>>,
    rate_limiter: Option<Arc<RateLimiter>>,
    chrome_client: Option<ChromeClient>,
    elicit_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<crate::tool::ElicitRequest>>>>,
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
        let sandbox_enabled = executor.sandbox_enabled();
        Self {
            registry,
            executor,
            context,
            cwd,
            sandbox_enabled,
            sandbox_override: None,
            task_store: None,
            cron_scheduler: None,
            rate_limiter: None,
            chrome_client: None,
            elicit_tx: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Enable or disable sandbox injection for dispatched tools.
    pub fn set_sandbox_enabled(&mut self, enabled: bool) {
        self.sandbox_enabled = enabled;
    }

    /// Set a custom sandbox config that overrides the default per-tool policy.
    pub fn set_sandbox_override(&mut self, config: Option<coderex_core::sandbox::SandboxConfig>) {
        self.sandbox_override = config;
    }

    /// Attach a shared task store for task tools.
    pub fn set_task_store(&mut self, store: Arc<TaskStore>) {
        self.task_store = Some(store);
    }

    /// Attach a shared cron scheduler for cron tools.
    pub fn set_cron_scheduler(&mut self, scheduler: Arc<CronScheduler>) {
        self.cron_scheduler = Some(scheduler);
    }

    /// Attach a shared rate limiter for web tools.
    pub fn set_rate_limiter(&mut self, limiter: Arc<RateLimiter>) {
        self.rate_limiter = Some(limiter);
    }

    /// Attach a Chrome CDP client for browser automation tools.
    pub fn set_chrome_client(&mut self, client: ChromeClient) {
        self.chrome_client = Some(client);
    }

    /// Remove the Chrome CDP client (used when --no-chrome is set).
    pub fn clear_chrome_client(&mut self) {
        self.chrome_client = None;
    }

    /// Attach an elicitation channel for mid-execution user questions.
    pub fn set_elicit_tx(&self, tx: Option<tokio::sync::mpsc::Sender<crate::tool::ElicitRequest>>) {
        if let Ok(mut guard) = self.elicit_tx.lock() {
            *guard = tx;
        }
    }
}

#[async_trait::async_trait]
impl CoreToolExecutor for RegistryToolDispatcher {
    fn list_tools(&self) -> Vec<coderex_core::tool_executor::ToolInfo> {
        self.registry
            .all_descriptors()
            .into_iter()
            .map(|d| coderex_core::tool_executor::ToolInfo {
                name: d.name.clone(),
                description: d
                    .input_schema
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&d.name)
                    .to_string(),
                input_schema: d.input_schema,
            })
            .collect()
    }

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
                        output: format!("tool '{tool_name}' not found in registry"),
                        is_error: true,
                    },
                };
            }
        };

        let mut ctx = ToolContext::new(self.context, &self.cwd);

        // Inject elicitation channel if available
        if let Ok(guard) = self.elicit_tx.lock() {
            if let Some(ref tx) = *guard {
                ctx = ctx.with_elicit_tx(tx.clone());
            }
        }

        // Build shared state map
        let mut extra = HashMap::new();

        // Inject sandbox config if enabled
        if self.sandbox_enabled {
            let config = self
                .sandbox_override
                .clone()
                .or_else(|| sandbox_policy::default_sandbox_config(tool_name, &self.cwd));
            if let Some(config) = config {
                extra.insert(
                    "sandbox_config".to_string(),
                    Box::new(config) as SharedState,
                );
            }
        }

        // Inject task store
        if let Some(ref store) = self.task_store {
            extra.insert(
                TASK_STORE_KEY.to_string(),
                Box::new(Arc::clone(store)) as SharedState,
            );
        }

        // Inject cron scheduler
        if let Some(ref scheduler) = self.cron_scheduler {
            extra.insert(
                CRON_SCHEDULER_KEY.to_string(),
                Box::new(Arc::clone(scheduler)) as SharedState,
            );
        }

        // Inject rate limiter
        if let Some(ref limiter) = self.rate_limiter {
            extra.insert(
                RATE_LIMITER_KEY.to_string(),
                Box::new(Arc::clone(limiter)) as SharedState,
            );
        }

        // Inject Chrome CDP client
        if let Some(ref client) = self.chrome_client {
            extra.insert(
                CHROME_CLIENT_KEY.to_string(),
                Box::new(client.clone()) as SharedState,
            );
        }

        if !extra.is_empty() {
            ctx = ctx.with_extra(extra);
        }

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
                    ToolError::RateLimited { name } => {
                        format!("tool '{name}' rate limit exceeded")
                    }
                    ToolError::ElicitationUnavailable => {
                        format!(
                            "tool '{tool_name}' requires user input but no elicitation channel is available"
                        )
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
