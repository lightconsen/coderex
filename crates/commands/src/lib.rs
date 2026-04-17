//! CodeRex slash command system — registry and all built-in commands.

#![warn(missing_docs)]

pub mod registry;

// Command modules (12.2 through 12.14)
mod session_commands;
mod config_commands;
mod stats_commands;
mod content_commands;
mod session_extension_commands;
mod config_extension_commands;
mod diagnostic_commands;
mod context_commands;
mod integration_commands;
mod git_commands;
mod debug_commands;
mod advanced_commands;
mod misc_commands;

// Re-export key types
pub use registry::{
    CommandContext, CommandHandler, CommandRegistry, CommandResult, CommandType,
    SlashCommand, register_all_commands,
};
