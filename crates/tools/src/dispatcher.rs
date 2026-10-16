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
    /// The working directory tools run in.
    ///
    /// Behind a shared lock rather than a plain field so a session that is
    /// moved into a worktree takes its tools with it — and shared rather than
    /// copied so a dispatcher clone (the agent manager holds one) sees the same
    /// move.
    cwd: Arc<parking_lot::RwLock<PathBuf>>,
    sandbox_enabled: bool,
    /// Whether this session's feature flags are on, which decides whether a
    /// gated tool is offered at all. Off until a session says otherwise: a
    /// tool registered to be withheld should be withheld by default.
    feature_flags_enabled: bool,
    sandbox_override: Option<coderex_core::sandbox::SandboxConfig>,
    task_store: Option<Arc<TaskStore>>,
    cron_scheduler: Option<Arc<CronScheduler>>,
    rate_limiter: Option<Arc<RateLimiter>>,
    chrome_client: Option<ChromeClient>,
    elicit_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<crate::tool::ElicitRequest>>>>,
}

impl RegistryToolDispatcher {
    /// The tool listing the model sees, by name.
    #[cfg(test)]
    fn listed_names(&self) -> Vec<String> {
        self.list_tools()
            .into_iter()
            .map(|info| info.name)
            .collect()
    }

    /// Tell the dispatcher whether the session's feature flags are on.
    pub const fn set_feature_flags_enabled(&mut self, enabled: bool) {
        self.feature_flags_enabled = enabled;
    }

    /// Move the working directory tools run in.
    ///
    /// Used when the session is given a checkout of its own — a worktree — so
    /// the tools work where the session says it is.
    pub fn set_cwd(&self, cwd: PathBuf) {
        *self.cwd.write() = cwd;
    }

    /// The working directory tools run in.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.cwd.read().clone()
    }

    /// The registry this dispatcher resolves tools from.
    ///
    /// Lets callers add tools discovered after construction (MCP servers).
    #[must_use]
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

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
            cwd: Arc::new(parking_lot::RwLock::new(cwd)),
            sandbox_enabled,
            feature_flags_enabled: false,
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
        // What the model is told about is what it can call. `all_descriptors`
        // is every registered tool, including the ones registered only to
        // refuse — thirteen of them answer "feature-gated and not enabled" —
        // and a tool the model is shown but cannot use is an invitation to
        // waste a turn discovering that.
        let available: std::collections::HashSet<String> = self
            .registry
            .available_tool_names(self.context, self.feature_flags_enabled)
            .into_iter()
            .collect();

        self.registry
            .all_descriptors()
            .into_iter()
            .filter(|d| d.is_enabled && available.contains(&d.name))
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

        // Deny rules are checked here, with the call's own arguments: a rule
        // written against a path can only be judged against the path this call
        // is about to touch. Until this existed, `deny_rules` were consulted
        // nowhere but the registry's own tests.
        let paths = path_arguments(input);
        if let Some(reason) = self.registry.denial_for(tool_name, &paths, self.context) {
            return ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: reason,
                    is_error: true,
                },
            };
        }

        // Read once rather than holding the lock across the call: the guard is
        // not `Send`, and this runs inside spawned tasks.
        let cwd = self.cwd.read().clone();
        let mut ctx = ToolContext::new(self.context, &cwd);

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
                .or_else(|| sandbox_policy::default_sandbox_config(tool_name, &cwd));
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
                    ToolError::Execution { name, message } => {
                        format!("tool '{name}' failed: {message}")
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

/// The paths a tool call is about to touch.
///
/// A deny rule written against a path needs the path, and only the tool knows
/// which argument carries it — so the ones that do are named here. Anything
/// else is scanned for values that look like paths, because for a deny rule
/// the safe direction to be wrong in is "matched".
#[must_use]
pub fn path_arguments(input: &Value) -> Vec<std::path::PathBuf> {
    const PATH_FIELDS: [&str; 8] = [
        "file_path",
        "notebook_path",
        "path",
        "target",
        "source",
        "directory",
        "cwd",
        "file",
    ];

    let mut paths = Vec::new();
    let Value::Object(map) = input else {
        return paths;
    };

    // The field the tool actually uses, when it is one we know.
    for field in PATH_FIELDS {
        if let Some(Value::String(value)) = map.get(field) {
            if !value.is_empty() {
                paths.push(std::path::PathBuf::from(value));
            }
        }
    }

    // Lists of files (Write's `files`, SendFile's `file_paths`).
    for field in ["files", "file_paths", "paths"] {
        if let Some(Value::Array(values)) = map.get(field) {
            for value in values {
                match value {
                    Value::String(path) => paths.push(std::path::PathBuf::from(path)),
                    Value::Object(entry) => {
                        if let Some(Value::String(path)) = entry.get("path") {
                            paths.push(std::path::PathBuf::from(path));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Nothing known: a rule is more useful over-matching than under-matching,
    // so any string that looks like a path is offered to it.
    if paths.is_empty() {
        for value in map.values() {
            if let Value::String(text) = value {
                if text.starts_with('/') || text.starts_with("~") || text.contains('/') {
                    paths.push(std::path::PathBuf::from(text));
                }
            }
        }
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::EchoTool;
    use crate::executor::ExecutionConfig;
    use crate::registry::ToolDenyRule;

    fn registry_with_echo() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        Arc::new(registry)
    }

    /// What the model is told about is what it can call: a tool that would answer
    /// "feature-gated and not enabled" is not offered.
    #[tokio::test]
    async fn the_listing_leaves_out_tools_that_would_refuse() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        crate::builtins::register_feature_gated_tools(&mut registry);
        let registry = Arc::new(registry);

        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        let listed = dispatcher.listed_names();
        assert!(listed.contains(&"echo".to_string()), "{listed:?}");
        assert!(
            !listed.iter().any(|name| name == "Monitor"),
            "a disabled tool was offered: {listed:?}"
        );
    }

    /// A tool a deny rule forbids is not offered either.
    #[tokio::test]
    async fn the_listing_leaves_out_denied_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.add_deny_rule(ToolDenyRule::for_tools(vec!["echo".into()]));
        let registry = Arc::new(registry);

        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        assert!(!dispatcher.listed_names().contains(&"echo".to_string()));
    }

    #[test]
    fn path_arguments_are_read_from_the_field_the_tool_uses() {
        assert_eq!(
            path_arguments(&serde_json::json!({"file_path": "/etc/passwd"})),
            vec![std::path::PathBuf::from("/etc/passwd")]
        );
        assert_eq!(
            path_arguments(&serde_json::json!({"file_path": "src/lib.rs"})),
            vec![std::path::PathBuf::from("src/lib.rs")]
        );
        assert_eq!(
            path_arguments(&serde_json::json!({"files": ["a.txt", "b.txt"]})),
            vec![
                std::path::PathBuf::from("a.txt"),
                std::path::PathBuf::from("b.txt")
            ]
        );
    }

    /// A rule is more useful over-matching than under-matching, so a tool whose
    /// path field is not known still offers anything that looks like a path.
    #[test]
    fn an_unknown_tool_still_offers_what_looks_like_a_path() {
        let found =
            path_arguments(&serde_json::json!({"where": "/etc/shadow", "name": "not-a-path"}));
        assert_eq!(found, vec![std::path::PathBuf::from("/etc/shadow")]);
    }

    #[test]
    fn arguments_with_no_paths_offer_nothing() {
        assert!(path_arguments(&serde_json::json!({"message": "hello"})).is_empty());
        assert!(path_arguments(&serde_json::json!({})).is_empty());
        assert!(path_arguments(&serde_json::json!("not an object")).is_empty());
    }

    /// The check has to happen at dispatch, with the call's own arguments: a
    /// rule written against a path is only meaningful against the path this
    /// call is about to touch.
    #[tokio::test]
    async fn a_path_deny_rule_stops_the_call_it_matches() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.add_deny_rule(ToolDenyRule::for_paths(vec!["**/secrets/**".into()]));
        let registry = Arc::new(registry);

        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        let denied = dispatcher
            .execute(
                "id-1",
                "echo",
                &serde_json::json!({"file_path": "/home/me/secrets/token", "message": "hi"}),
            )
            .await;
        assert!(denied.result.is_error, "{:?}", denied.result);
        assert!(
            denied.result.output.contains("secrets/token"),
            "{}",
            denied.result.output
        );

        // The same call, touching somewhere else, runs.
        let allowed = dispatcher
            .execute(
                "id-2",
                "echo",
                &serde_json::json!({"file_path": "/home/me/notes.md", "message": "hi"}),
            )
            .await;
        assert!(!allowed.result.is_error, "{:?}", allowed.result);
        assert_eq!(allowed.result.output, "hi");
    }

    #[tokio::test]
    async fn a_tool_deny_rule_stops_the_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.add_deny_rule(ToolDenyRule::for_tools(vec!["echo".into()]));
        let registry = Arc::new(registry);

        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        let denied = dispatcher
            .execute("id-1", "echo", &serde_json::json!({"message": "hi"}))
            .await;
        assert!(denied.result.is_error, "{:?}", denied.result);
        assert!(
            denied.result.output.contains("denied"),
            "{}",
            denied.result.output
        );
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
