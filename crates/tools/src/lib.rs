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
mod task_store;
mod todo_store;
mod tool;
mod tool_cache;

pub use builtins::{
    AgentTool, AskUserQuestionTool, BashTool, BriefTool, ChromeClient, ConfigTool, CronCreateTool,
    CronDeleteTool, CronListTool, CurrentDirTool, EchoTool, EditTool, EnterPlanModeTool,
    ExitPlanModeTool, GlobTool, GrepTool, ListDirTool, LspTool, McpAuthTool, NoOpTool,
    NotebookEditTool, PowerShellTool, ReadTool, RemoteTriggerTool, ReplTool, SendMessageTool,
    SkillTool, SleepTool, StructuredOutputTool, SyntheticOutputTool, TaskCreateTool, TaskGetTool,
    TaskListTool, TaskOutputTool, TaskStopTool, TaskUpdateTool, TodoWriteTool, ToolSearchTool,
    WebFetchTool, WebSearchTool, WriteTool, default_token_path, persist_token_at,
    register_builtin_tools,
};
pub use context::{ExecutionContext, ToolError, ToolOutput};
pub use cron_scheduler::{CRON_SCHEDULER_KEY, CronJob, CronScheduler};
pub use dispatcher::RegistryToolDispatcher;
pub use executor::{ExecutionConfig, ToolExecutor};
pub use rate_limiter::{RATE_LIMITER_KEY, RateLimitConfig, RateLimiter};
pub use registry::{ToolDenyRule, ToolNotFoundError, ToolRegistry};
pub use sandbox_policy::{default_sandbox_config, is_path_allowed};
pub use task_store::{TASK_STORE_KEY, TaskRecord, TaskStatus, TaskStore};
pub use todo_store::{TODO_STORE_KEY, TodoItem, TodoStatus, TodoStore};
pub use tool::{
    ElicitRequest, InterruptBehavior, Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier,
};
pub use tool_cache::{ToolCache, ToolCacheConfig};
