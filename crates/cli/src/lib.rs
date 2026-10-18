//! CodeRex CLI library — REPL loop, orchestrator, and CLI utilities.

pub mod agent_selection;
pub mod bridge_launcher;
pub mod cli;
pub mod daemon;
pub mod from_pr;
pub mod hook_actions;
pub mod ide;
pub mod interactive_e2e;
pub mod mcp_elicitation;
pub mod mcp_reauth;
pub mod memory_tools;
pub mod orchestrator;
pub mod recovery;
pub mod repl;
pub mod session_resume;
pub mod tmux;
pub mod tracing_setup;
pub mod version;
pub mod welcome;

/// Serialises tests that mutate process-wide environment variables (`PATH`,
/// the IDE env vars). `std::env::set_var` is unsafe in edition 2024 for
/// exactly this reason, so every such test in this crate takes this lock.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub use cli::{Cli, Command, parse_args};
pub use repl::ReplLoop;
