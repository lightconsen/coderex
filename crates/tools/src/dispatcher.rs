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
use crate::todo_store::{TODO_STORE_KEY, TodoStore};
use crate::tool::{
    Backgrounding, JOB_SESSION_ID_KEY, RuleContentField, SharedState, ToolCapabilities,
    ToolContext, ToolInput,
};
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
    ///
    /// Shared across clones because `--betas` is resolved after the dispatcher
    /// (and the agent manager's clone of it) already exists.
    feature_flags_enabled: Arc<std::sync::atomic::AtomicBool>,
    sandbox_override: Option<coderex_core::sandbox::SandboxConfig>,
    task_store: Option<Arc<TaskStore>>,
    /// The session's todo list, held for the life of this dispatcher and
    /// shared by every clone of it. `TodoWrite` writes here and the next call
    /// (or anyone holding the dispatcher) sees the result.
    todo_store: Arc<TodoStore>,
    cron_scheduler: Option<Arc<CronScheduler>>,
    rate_limiter: Option<Arc<RateLimiter>>,
    chrome_client: Option<ChromeClient>,
    /// Whether Chrome tools are offered. Shared across clones: the engine and
    /// the agent manager each hold a dispatcher clone, and `--no-chrome`
    /// arrives after both were made, so a plain copy cannot be flipped.
    chrome_enabled: Arc<std::sync::atomic::AtomicBool>,
    elicit_tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<crate::tool::ElicitRequest>>>>,
    /// Session that owns work started by these tools, when known. Tools that
    /// write durable job records use it to attribute the record.
    session_id: Option<String>,
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
    pub fn set_feature_flags_enabled(&self, enabled: bool) {
        self.feature_flags_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
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
            feature_flags_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sandbox_override: None,
            task_store: None,
            todo_store: Arc::new(TodoStore::new()),
            cron_scheduler: None,
            rate_limiter: None,
            chrome_client: None,
            chrome_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            elicit_tx: Arc::new(std::sync::Mutex::new(None)),
            session_id: None,
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

    /// Attach the owning session id, used by tools that write durable job
    /// records (background shell commands).
    pub fn set_session_id(&mut self, session_id: impl Into<String>) {
        self.session_id = Some(session_id.into());
    }

    /// Attach a shared task store for task tools.
    pub fn set_task_store(&mut self, store: Arc<TaskStore>) {
        self.task_store = Some(store);
    }

    /// The session's todo list, as last written by `TodoWrite`.
    ///
    /// This is the same store the tools see in their context, so a caller can
    /// read what a session wrote without building a `ToolContext` itself.
    #[must_use]
    pub fn todo_store(&self) -> &Arc<TodoStore> {
        &self.todo_store
    }

    /// Attach a shared cron scheduler for cron tools.
    pub fn set_cron_scheduler(&mut self, scheduler: Arc<CronScheduler>) {
        self.cron_scheduler = Some(scheduler);
    }

    /// The shared cron scheduler, when one is attached.
    ///
    /// The REPL uses it to report a fired job back to the scheduler so its
    /// durable definition (and its job record) stays current.
    #[must_use]
    pub fn cron_scheduler(&self) -> Option<Arc<CronScheduler>> {
        self.cron_scheduler.clone()
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

    /// Turn the Chrome tool family on or off for every clone of this
    /// dispatcher.
    pub fn set_chrome_enabled(&self, enabled: bool) {
        self.chrome_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the Chrome tool family is offered.
    #[must_use]
    pub fn chrome_enabled(&self) -> bool {
        self.chrome_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
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
        // is every registered tool, including disabled ones; a tool the model
        // is shown but cannot use is an invitation to waste a turn discovering
        // that.
        let feature_flags_enabled = self
            .feature_flags_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        let available: std::collections::HashSet<String> = self
            .registry
            .available_tool_names(self.context, feature_flags_enabled)
            .into_iter()
            .collect();

        // Chrome is a session capability (`--no-chrome`), not one of the
        // general feature flags; a session with no flags still has Chrome
        // unless it turned it off. Build its candidate set with the feature
        // gate forced on, though, so everything else the registry filters on
        // — deny rules, the context allowlist, and the execution context's
        // own rules (PlanMode is read-only, AsyncAgent excludes approval
        // tools) — still applies. Listing a Chrome tool the execute path will
        // refuse only spends a turn discovering that.
        let chrome_available: std::collections::HashSet<String> = self
            .registry
            .available_tool_names(self.context, true)
            .into_iter()
            .collect();

        let chrome_enabled = self.chrome_enabled();
        self.registry
            .all_descriptors()
            .into_iter()
            .filter(|d| {
                if !d.is_enabled {
                    return false;
                }
                if d.name.starts_with("chrome_") {
                    return chrome_enabled && chrome_available.contains(&d.name);
                }
                available.contains(&d.name)
            })
            .map(|d| {
                let caps = self
                    .registry
                    .get_tool_arc(&d.name)
                    .map_or_else(ToolCapabilities::default, |tool| tool.capabilities());
                coderex_core::tool_executor::ToolInfo {
                    name: d.name.clone(),
                    description: d
                        .input_schema
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&d.name)
                        .to_string(),
                    input_schema: advertised_schema(d.input_schema, caps),
                }
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

        let is_chrome = tool_name.starts_with("chrome_");
        if is_chrome && !self.chrome_enabled() {
            return ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: "Chrome tools are disabled for this session (--no-chrome)".into(),
                    is_error: true,
                },
            };
        }

        // Deny rules are checked here, with the call's own arguments: a rule
        // written against a path can only be judged against the path this call
        // is about to touch. The tool says which argument carries that value;
        // one that says nothing is read the old way. Until this existed,
        // `deny_rules` were consulted nowhere but the registry's own tests.
        let caps = tool.capabilities();
        let paths = rule_content_arguments(caps.rule_content_field, input);
        if let Some(reason) = self.registry.denial_for(tool_name, &paths, self.context) {
            return ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: reason,
                    is_error: true,
                },
            };
        }

        // The listing and the execute path have to agree on which Chrome
        // tools exist for this turn. The feature flag is forced on (Chrome is
        // a session capability, not a beta flag), which leaves the registry's
        // deny rules and execution-context filters in charge — the same set
        // `list_tools` consults for Chrome.
        if is_chrome {
            let available = self.registry.available_tool_names(self.context, true);
            if !available.iter().any(|name| name == tool_name) {
                return ToolDispatchOutcome {
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_string(),
                        output: format!(
                            "tool '{tool_name}' is not available in this session's execution \
                             context"
                        ),
                        is_error: true,
                    },
                };
            }
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

        // The session's todo store: every call through this dispatcher (and
        // its clones) sees the list the last `TodoWrite` wrote. Without this
        // the tool reports a write that went nowhere.
        extra.insert(
            TODO_STORE_KEY.to_string(),
            Box::new(Arc::clone(&self.todo_store)) as SharedState,
        );

        // The live registry, so `ToolSearch` searches the tools this session
        // actually has rather than an empty map.
        extra.insert(
            "tool_registry".to_string(),
            Box::new(Arc::clone(&self.registry)) as SharedState,
        );

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

        // Inject the owning session id for durable job records.
        if let Some(ref session_id) = self.session_id {
            extra.insert(
                JOB_SESSION_ID_KEY.to_string(),
                Box::new(session_id.clone()) as SharedState,
            );
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

/// The schema the model is shown, with promises the tool does not keep removed.
///
/// A tool that advertises `run_in_background` but declares no backgrounding
/// behaviour would silently ignore the flag, and a tool the model is told
/// something false about is worse than one that says nothing: the model spends
/// a turn discovering the truth. Until such a tool honours the flag, the
/// argument is not offered. A tool that declares [`Backgrounding::SelfManaged`]
/// or [`Backgrounding::Detach`] keeps it.
#[must_use]
fn advertised_schema(mut schema: Value, caps: ToolCapabilities) -> Value {
    if caps.backgrounding != Backgrounding::None {
        return schema;
    }
    let advertised = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .is_some_and(|properties| properties.remove("run_in_background").is_some());
    if !advertised {
        return schema;
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|entry| entry.as_str() != Some("run_in_background"));
    }
    schema
}

/// The values a permission rule should be judged against for this call.
///
/// A tool that declares its own [`RuleContentField`] is read from exactly that
/// argument and nothing else, so a rule cannot be tripped by an unrelated
/// value that happens to sit in the same call. A tool that declares nothing
/// keeps the old behaviour: the known path fields, then anything path-shaped,
/// because for a deny rule the safe direction to be wrong in is "matched".
#[must_use]
pub fn rule_content_arguments(
    field: Option<RuleContentField>,
    input: &Value,
) -> Vec<std::path::PathBuf> {
    match field {
        Some(RuleContentField::Scalar(name)) => scalar_argument(input, name).into_iter().collect(),
        Some(RuleContentField::List(name)) => list_argument(input, name),
        None => path_arguments(input),
    }
}

/// One string argument, if it is a non-empty string.
fn scalar_argument(input: &Value, field: &str) -> Option<std::path::PathBuf> {
    let Value::Object(map) = input else {
        return None;
    };
    match map.get(field) {
        Some(Value::String(value)) if !value.is_empty() => Some(std::path::PathBuf::from(value)),
        _ => None,
    }
}

/// A list argument: strings, or objects carrying a `path`.
fn list_argument(input: &Value, field: &str) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    let Value::Object(map) = input else {
        return paths;
    };
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
    paths
}

/// The paths a tool call is about to touch, for a tool that declares none.
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
        paths.extend(scalar_argument(input, field));
    }

    // Lists of files (Write's `files`, SendFile's `file_paths`).
    for field in ["files", "file_paths", "paths"] {
        paths.extend(list_argument(input, field));
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
    use crate::context::ToolOutput;
    use crate::executor::ExecutionConfig;
    use crate::registry::ToolDenyRule;
    use crate::tool::{InterruptBehavior, Tool, ToolDescriptor, ToolTier};

    /// A tool whose schema advertises `run_in_background` without declaring
    /// any backgrounding behaviour, as a copied schema would.
    struct AdvertisesButIgnoresBackground;

    #[async_trait::async_trait]
    impl Tool for AdvertisesButIgnoresBackground {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "ignores_background".into(),
                tier: ToolTier::Baseline,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "run_in_background": { "type": "boolean" }
                    }
                }),
                output_schema: serde_json::json!({}),
                requires_approval: false,
                is_enabled: true,
                is_read_only: true,
                is_concurrency_safe: true,
                interrupt_behavior: InterruptBehavior::Interruptible,
            }
        }

        async fn execute(
            &self,
            _input: ToolInput,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: String::new(),
                is_display: false,
            })
        }
    }

    /// A tool registered but disabled: the model must not be offered it.
    struct DisabledTool;

    #[async_trait::async_trait]
    impl Tool for DisabledTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "disabled".into(),
                tier: ToolTier::Baseline,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
                output_schema: serde_json::json!({}),
                requires_approval: false,
                is_enabled: false,
                is_read_only: true,
                is_concurrency_safe: true,
                interrupt_behavior: InterruptBehavior::RunToCompletion,
            }
        }

        async fn execute(
            &self,
            _input: ToolInput,
            _ctx: ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                output: "tool is disabled".into(),
                is_display: true,
            })
        }
    }

    fn registry_with_echo() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        Arc::new(registry)
    }

    /// What the model is told about is what it can call: a disabled tool is not
    /// offered.
    #[tokio::test]
    async fn the_listing_leaves_out_tools_that_would_refuse() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(DisabledTool));
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
            !listed.iter().any(|name| name == "disabled"),
            "a disabled tool was offered: {listed:?}"
        );
    }

    /// Chrome is a session capability (`--no-chrome`), not a general feature
    /// flag: it is offered with no flags set, and withheld when the session
    /// turns it off.
    #[test]
    fn chrome_tools_follow_the_session_capability() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::builtins::chrome::ChromeListTabsTool));
        registry.register(Arc::new(crate::builtins::chrome::ChromeNavigateTool));
        let registry = Arc::new(registry);

        let dispatcher = RegistryToolDispatcher::new(
            registry,
            Arc::new(InnerExecutor::new(ExecutionConfig::default())),
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        // No feature flags were set: Chrome is still offered by default.
        let listed = dispatcher.listed_names();
        assert!(listed.contains(&"chrome_list_tabs".into()), "{listed:?}");
        assert!(listed.contains(&"chrome_navigate".into()), "{listed:?}");

        dispatcher.set_chrome_enabled(false);
        let listed = dispatcher.listed_names();
        assert!(!listed.contains(&"chrome_list_tabs".into()), "{listed:?}");
        assert!(!listed.contains(&"chrome_navigate".into()), "{listed:?}");
    }

    /// The Chrome bypass must not bypass deny rules: a name-based rule hides
    /// the tool from the listing and refuses the call.
    #[tokio::test]
    async fn a_deny_rule_removes_a_chrome_tool_from_listing_and_execution() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::builtins::chrome::ChromeListTabsTool));
        registry.add_deny_rule(ToolDenyRule::for_tools(vec!["chrome_list_tabs".into()]));
        let registry = Arc::new(registry);

        let dispatcher = RegistryToolDispatcher::new(
            registry,
            Arc::new(InnerExecutor::new(ExecutionConfig::default())),
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        assert!(
            !dispatcher
                .listed_names()
                .contains(&"chrome_list_tabs".into()),
            "a denied Chrome tool was offered: {:?}",
            dispatcher.listed_names()
        );

        let outcome = dispatcher
            .execute("id-1", "chrome_list_tabs", &serde_json::json!({}))
            .await;
        assert!(outcome.result.is_error, "{:?}", outcome.result);
        assert!(
            outcome.result.output.contains("denied"),
            "{}",
            outcome.result.output
        );
    }

    /// PlanMode is read-only, Chrome included. The mutating tool is hidden
    /// and refused; the read-only one stays listed.
    #[tokio::test]
    async fn plan_mode_hides_mutating_chrome_tools_and_keeps_read_only_ones() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::builtins::chrome::ChromeListTabsTool));
        registry.register(Arc::new(crate::builtins::chrome::ChromeNavigateTool));
        let registry = Arc::new(registry);

        // Pin the descriptors the filtering decision rests on.
        assert!(
            registry
                .get_tool("chrome_list_tabs")
                .expect("registered")
                .descriptor()
                .is_read_only
        );
        assert!(
            !registry
                .get_tool("chrome_navigate")
                .expect("registered")
                .descriptor()
                .is_read_only
        );

        let dispatcher = RegistryToolDispatcher::new(
            registry,
            Arc::new(InnerExecutor::new(ExecutionConfig::default())),
            ExecutionContext::PlanMode,
            PathBuf::from("/tmp"),
        );

        let listed = dispatcher.listed_names();
        assert!(
            listed.contains(&"chrome_list_tabs".into()),
            "the read-only Chrome tool was hidden: {listed:?}"
        );
        assert!(
            !listed.contains(&"chrome_navigate".into()),
            "a mutating Chrome tool was offered in PlanMode: {listed:?}"
        );

        let outcome = dispatcher
            .execute(
                "id-1",
                "chrome_navigate",
                &serde_json::json!({"url": "https://example.com"}),
            )
            .await;
        assert!(outcome.result.is_error, "{:?}", outcome.result);
        assert!(
            outcome.result.output.contains("not available"),
            "{}",
            outcome.result.output
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

    /// A Bash call is judged against its command line, because that is what
    /// the tool declares its rule content to be. This command has no slash in
    /// it, so the path-shaped fallback would have offered nothing to the rule —
    /// which is what makes this a regression test for the declaration rather
    /// than for the dispatch check.
    #[tokio::test]
    async fn a_bash_command_without_a_slash_is_still_judged() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::builtins::BashTool::new()));
        registry.add_deny_rule(ToolDenyRule::for_paths(vec!["*rm -rf*".into()]));
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
                "Bash",
                &serde_json::json!({"command": "rm -rf build"}),
            )
            .await;
        assert!(denied.result.is_error, "{:?}", denied.result);
        assert!(
            denied.result.output.contains("denied"),
            "the refusal should say so: {}",
            denied.result.output
        );
    }

    /// The control for the test above: a command the rule does not match runs.
    #[tokio::test]
    async fn a_bash_command_the_rule_does_not_match_runs() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::builtins::BashTool::new()));
        registry.add_deny_rule(ToolDenyRule::for_paths(vec!["*rm -rf*".into()]));
        let registry = Arc::new(registry);

        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        let allowed = dispatcher
            .execute(
                "id-1",
                "Bash",
                &serde_json::json!({"command": "echo hello"}),
            )
            .await;
        assert!(!allowed.result.is_error, "{:?}", allowed.result);
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

    /// A tool that names its own rule content is read from exactly that
    /// argument — no other value in the same call can trip a rule.
    #[test]
    fn a_declared_field_is_the_only_thing_a_rule_sees() {
        let input = serde_json::json!({
            "file_path": "/etc/passwd",
            "content": "/etc/shadow",
        });

        assert_eq!(
            rule_content_arguments(Some(RuleContentField::Scalar("file_path")), &input),
            vec![std::path::PathBuf::from("/etc/passwd")]
        );
        // Declared as something this call does not carry: nothing is offered,
        // and in particular the path-shaped `file_path` is not.
        assert!(
            rule_content_arguments(Some(RuleContentField::Scalar("command")), &input).is_empty()
        );

        // Undeclared, the heuristic reaches a path-shaped value even when the
        // tool's own field is not one it knows — the over-matching this
        // declaration exists to stop.
        let opaque = serde_json::json!({"blob": "/etc/shadow"});
        assert_eq!(
            path_arguments(&opaque),
            vec![std::path::PathBuf::from("/etc/shadow")]
        );
        assert!(
            rule_content_arguments(Some(RuleContentField::Scalar("command")), &opaque).is_empty()
        );
    }

    #[test]
    fn a_declared_list_field_is_read_as_a_list() {
        let input = serde_json::json!({"files": ["a.txt", {"path": "b.txt"}, 7]});
        assert_eq!(
            rule_content_arguments(Some(RuleContentField::List("files")), &input),
            vec![
                std::path::PathBuf::from("a.txt"),
                std::path::PathBuf::from("b.txt")
            ]
        );
    }

    #[test]
    fn an_undeclared_tool_keeps_the_path_heuristic() {
        let input = serde_json::json!({"where": "/etc/shadow"});
        assert_eq!(rule_content_arguments(None, &input), path_arguments(&input));
    }

    /// The tool that declares the field is the one a deny rule is judged
    /// against: `Echo`-shaped input where the declared field is the only one
    /// that matters.
    struct DeclaredPathTool;

    #[async_trait::async_trait]
    impl crate::tool::Tool for DeclaredPathTool {
        fn descriptor(&self) -> crate::tool::ToolDescriptor {
            crate::tool::ToolDescriptor {
                name: "declared".into(),
                tier: crate::tool::ToolTier::Baseline,
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                requires_approval: false,
                is_enabled: true,
                is_read_only: false,
                is_concurrency_safe: true,
                interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
            }
        }

        fn capabilities(&self) -> ToolCapabilities {
            ToolCapabilities {
                rule_content_field: Some(RuleContentField::Scalar("command")),
                ..ToolCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _input: ToolInput,
            _ctx: ToolContext<'_>,
        ) -> Result<crate::context::ToolOutput, crate::context::ToolError> {
            Ok(crate::context::ToolOutput {
                output: "ran".into(),
                is_display: false,
            })
        }
    }

    /// A shell-shaped tool that declares `command` as its rule content is
    /// stopped by a rule over that value — the case the hardcoded field list
    /// could not express.
    #[tokio::test]
    async fn a_declared_command_field_drives_the_deny_check() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(DeclaredPathTool));
        registry.add_deny_rule(ToolDenyRule::for_paths(vec!["/etc/shadow".into()]));
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
                "declared",
                &serde_json::json!({"command": "/etc/shadow"}),
            )
            .await;
        assert!(denied.result.is_error, "{:?}", denied.result);
        assert!(
            denied.result.output.contains("/etc/shadow"),
            "{}",
            denied.result.output
        );

        let allowed = dispatcher
            .execute(
                "id-2",
                "declared",
                &serde_json::json!({"command": "/tmp/ok"}),
            )
            .await;
        assert!(!allowed.result.is_error, "{:?}", allowed.result);

        // The declared field is the *only* one judged: a denied path sitting in
        // some other argument of a tool that named `command` is not its rule
        // content, and must not stop the call.
        let unrelated = dispatcher
            .execute(
                "id-3",
                "declared",
                &serde_json::json!({"command": "/tmp/ok", "cwd": "/etc/shadow"}),
            )
            .await;
        assert!(!unrelated.result.is_error, "{:?}", unrelated.result);
    }

    /// A tool that never reads `run_in_background` must not be advertised as
    /// taking it: the model would spend a turn finding out.
    #[tokio::test]
    async fn a_tool_that_ignores_run_in_background_does_not_advertise_it() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(AdvertisesButIgnoresBackground));
        registry.register(Arc::new(crate::builtins::BashTool::new()));
        registry.register(Arc::new(crate::builtins::AgentTool::new()));
        let registry = Arc::new(registry);

        let executor = Arc::new(InnerExecutor::new(ExecutionConfig::default()));
        let dispatcher = RegistryToolDispatcher::new(
            registry,
            executor,
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        let tools = dispatcher.list_tools();
        let schema_of = |name: &str| {
            tools
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.input_schema.clone())
                .unwrap_or_else(|| panic!("{name} was not listed"))
        };

        // This one advertises the argument and never reads it, so it is gone.
        assert!(
            schema_of("ignores_background")
                .get("properties")
                .unwrap()
                .get("run_in_background")
                .is_none(),
            "a tool that ignores the flag still advertises it"
        );
        // Bash and Agent read it and really do background, so they keep it.
        assert!(
            schema_of("Bash")
                .get("properties")
                .unwrap()
                .get("run_in_background")
                .is_some(),
            "Bash honours run_in_background and must keep advertising it"
        );
        assert!(
            schema_of("Agent")
                .get("properties")
                .unwrap()
                .get("run_in_background")
                .is_some(),
            "Agent honours run_in_background and must keep advertising it"
        );
    }

    /// Two `TodoWrite` calls through the dispatcher a session actually uses,
    /// and the store it wrote to is the dispatcher's own. Without the store in
    /// `extra`, the tool still reports "Wrote N todo(s)" and the second call
    /// has nothing to update.
    #[tokio::test]
    async fn todo_write_persists_across_dispatched_calls() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::builtins::TodoWriteTool));
        let dispatcher = RegistryToolDispatcher::new(
            Arc::new(registry),
            Arc::new(InnerExecutor::new(ExecutionConfig::default())),
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        let first = dispatcher
            .execute(
                "id-1",
                "TodoWrite",
                &serde_json::json!({"todos": [
                    {"subject": "write the fix", "status": "in_progress"}
                ]}),
            )
            .await;
        assert!(!first.result.is_error, "{:?}", first.result);
        let after_first = dispatcher.todo_store().get_all();
        assert_eq!(after_first.len(), 1, "{after_first:?}");
        assert_eq!(after_first[0].subject, "write the fix");
        assert_eq!(
            after_first[0].status,
            crate::todo_store::TodoStatus::InProgress
        );

        let second = dispatcher
            .execute(
                "id-2",
                "TodoWrite",
                &serde_json::json!({"todos": [
                    {"subject": "write the fix", "status": "completed"},
                    {"subject": "add the test", "status": "pending"}
                ]}),
            )
            .await;
        assert!(!second.result.is_error, "{:?}", second.result);

        let after_second = dispatcher.todo_store().get_all();
        assert_eq!(after_second.len(), 2, "{after_second:?}");
        assert_eq!(after_second[0].subject, "write the fix");
        assert_eq!(
            after_second[0].status,
            crate::todo_store::TodoStatus::Completed
        );
        assert_eq!(after_second[1].subject, "add the test");

        // A clone of the dispatcher is the same session, so it sees the same
        // list rather than a fresh empty one.
        assert_eq!(dispatcher.clone().todo_store().get_all().len(), 2);
    }

    /// `ToolSearch` searches the live registry the dispatcher was built with.
    /// Without it in `extra`, every query answers "No tools found".
    #[tokio::test]
    async fn tool_search_finds_a_registered_tool_through_the_dispatcher() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(crate::builtins::ToolSearchTool));
        let dispatcher = RegistryToolDispatcher::new(
            Arc::new(registry),
            Arc::new(InnerExecutor::new(ExecutionConfig::default())),
            ExecutionContext::Normal,
            PathBuf::from("/tmp"),
        );

        let outcome = dispatcher
            .execute("id-1", "ToolSearch", &serde_json::json!({"query": "echo"}))
            .await;
        assert!(!outcome.result.is_error, "{:?}", outcome.result);
        assert!(
            !outcome.result.output.contains("No tools found"),
            "{}",
            outcome.result.output
        );
        assert!(
            outcome.result.output.contains("**echo**"),
            "{}",
            outcome.result.output
        );
    }
}
