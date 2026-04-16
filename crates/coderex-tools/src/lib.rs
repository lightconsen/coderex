//! Tool trait, tool registry, execution, and built-in tools.

#![warn(missing_docs)]

mod tool;
mod registry;
mod context;
mod builtins;
mod executor;

pub use tool::{Tool, ToolInput, InterruptBehavior, ToolTier};
pub use registry::{ToolRegistry, ToolDenyRule, ToolNotFoundError};
pub use context::{ExecutionContext, ToolError, ToolOutput};
pub use builtins::{EchoTool, NoOpTool};
pub use executor::{ToolExecutor, ExecutionConfig};
