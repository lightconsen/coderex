//! CodeRex CLI library — REPL loop, orchestrator, and CLI utilities.

pub mod cli;
pub mod orchestrator;
pub mod repl;
pub mod tracing_setup;

pub use cli::{parse_args, Cli, Command};
pub use repl::ReplLoop;
