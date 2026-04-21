//! CodeRex CLI library — REPL loop, orchestrator, and CLI utilities.

pub mod cli;
pub mod daemon;
pub mod orchestrator;
pub mod recovery;
pub mod repl;
pub mod tracing_setup;
pub mod version;
pub mod welcome;

pub use cli::{parse_args, Cli, Command};
pub use repl::ReplLoop;
