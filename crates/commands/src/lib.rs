//! CodeRex slash command system — registry and all built-in commands.

#![warn(missing_docs)]

pub mod registry;

// Command modules (12.2 through 12.14)
mod advanced_commands;
mod config_commands;
mod config_extension_commands;
mod content_commands;
mod context_commands;
mod debug_commands;
mod diagnostic_commands;
mod git_commands;
mod history;
mod integration_commands;
mod macro_commands;
mod misc_commands;
mod session_commands;
mod session_extension_commands;
mod stats_commands;

// Re-export key types
pub use history::CommandHistory;
pub use macro_commands::MacroRegistry;
pub use registry::{
    CommandArgs, CommandContext, CommandHandler, CommandRegistry, CommandResult, CommandType,
    SlashCommand, register_all_commands,
};
