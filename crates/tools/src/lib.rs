//! Tool trait, tool registry, execution, and built-in tools.

#![warn(missing_docs)]

mod tool;
mod registry;
mod context;
mod builtins;
mod executor;
mod dispatcher;
mod sandbox_policy;
mod todo_store;
mod task_store;
mod cron_scheduler;
mod rate_limiter;
mod tool_cache;

pub use tool::{Tool, ToolInput, InterruptBehavior, ToolTier};
pub use registry::{ToolRegistry, ToolDenyRule, ToolNotFoundError};
pub use context::{ExecutionContext, ToolError, ToolOutput};
pub use builtins::{
    AgentTool, AskUserQuestionTool, BashTool, BriefTool, ConfigTool, CronCreateTool, CronDeleteTool,
    CronListTool, EchoTool, EditTool, EnterPlanModeTool, ExitPlanModeTool, GlobTool, GrepTool,
    LspTool, McpAuthTool, NoOpTool, NotebookEditTool, PowerShellTool, ReadTool, RemoteTriggerTool, ReplTool, SendMessageTool,
    SkillTool, SleepTool, StructuredOutputTool, SyntheticOutputTool, TaskCreateTool, TaskGetTool,
    TaskListTool, TaskOutputTool, TaskStopTool, TaskUpdateTool, TodoWriteTool, ToolSearchTool,
    WebFetchTool, WebSearchTool, WriteTool,
    register_builtin_tools,
};
pub use executor::{ToolExecutor, ExecutionConfig};
pub use dispatcher::RegistryToolDispatcher;
pub use sandbox_policy::{default_sandbox_config, is_path_allowed};
pub use todo_store::{TodoItem, TodoStatus, TodoStore, TODO_STORE_KEY};
pub use task_store::{TaskRecord, TaskStatus, TaskStore, TASK_STORE_KEY};
pub use cron_scheduler::{CronJob, CronScheduler, CRON_SCHEDULER_KEY};
pub use rate_limiter::{RateLimiter, RateLimitConfig, RATE_LIMITER_KEY};
pub use tool_cache::{ToolCache, ToolCacheConfig};
