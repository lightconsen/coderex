//! Tool trait, tool registry, execution, and built-in tools.

#![warn(missing_docs)]

mod builtins;
mod context;
mod cron_scheduler;
mod dispatcher;
mod executor;
mod rate_limiter;
mod registry;
mod sandbox_policy;
mod sandbox_shell;
mod task_store;
mod todo_store;
mod tool;
mod tool_cache;
mod verified_fs;

pub use builtins::{
    AgentTool, AskUserQuestionTool, BashTool, BriefTool, ChromeClient, ConfigTool, CronCreateTool,
    CronDeleteTool, CronListTool, CurrentDirTool, EchoTool, EditTool, EnterPlanModeTool,
    ExitPlanModeTool, GlobTool, GrepTool, ListDirTool, LspTool, McpAuthTool, NoOpTool,
    NotebookEditTool, PowerShellTool, ReadTool, RemoteTriggerTool, ReplTool, SendMessageTool,
    SkillTool, SleepTool, StructuredOutputTool, SyntheticOutputTool, TaskCreateTool, TaskGetTool,
    TaskListTool, TaskOutputTool, TaskStopTool, TaskUpdateTool, TodoWriteTool, ToolSearchTool,
    WebFetchTool, WebSearchTool, WriteTool, cache_discovery_at, default_token_path,
    persist_token_at, persist_token_with_endpoints_at, platform_shell_is_powershell,
    platform_shell_tool, register_builtin_tools, register_shell_tool,
};
pub use coderex_core::permissions::ToolRuleExemptions;
pub use context::{ExecutionContext, ToolError, ToolOutput};
pub use cron_scheduler::{CRON_SCHEDULER_KEY, CronJob, CronScheduler};
pub use dispatcher::RegistryToolDispatcher;
pub use executor::{ExecutionConfig, ToolExecutor};
pub use rate_limiter::{RATE_LIMITER_KEY, RateLimitConfig, RateLimiter};
pub use registry::{ToolDenyRule, ToolNotFoundError, ToolRegistry, install_rule_exemptions};
pub use sandbox_policy::{default_sandbox_config, is_path_allowed};
pub use task_store::{TASK_STORE_KEY, TaskRecord, TaskStatus, TaskStore};
pub use todo_store::{TODO_STORE_KEY, TodoItem, TodoStatus, TodoStore};
pub use tool::{
    Backgrounding, ElicitRequest, InterruptBehavior, MaxResultSizeChars, RuleContentField, Tool,
    ToolCapabilities, ToolContext, ToolDescriptor, ToolInput, ToolTier, fire_tool_hook,
};
pub use tool_cache::{ToolCache, ToolCacheConfig};

/// The feature flag a tool needs to be offered.
///
/// Feature-gated tools are hidden from the listing and refused on execution
/// while their flag is off. A gated tool that names no flag keeps the generic
/// gate ("any feature flag is on"), which is how the Chrome family behaves —
/// it is a session capability rather than a beta flag.
#[must_use]
pub fn required_feature_flag(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "WebFetch" => Some("web_browser_tool"),
        _ => None,
    }
}
