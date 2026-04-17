//! Tool trait, tool registry, execution, and built-in tools.

#![warn(missing_docs)]

mod tool;
mod registry;
mod context;
mod builtins;
mod executor;
mod todo_store;
mod task_store;
mod cron_scheduler;

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
pub use todo_store::{TodoItem, TodoStatus, TodoStore, TODO_STORE_KEY};
pub use task_store::{TaskRecord, TaskStatus, TaskStore, TASK_STORE_KEY};
pub use cron_scheduler::{CronJob, CronScheduler, CRON_SCHEDULER_KEY};
