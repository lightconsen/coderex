//! CodeRex CLI library — REPL loop, orchestrator, and CLI utilities.

pub mod cli;
pub mod daemon;
pub mod interactive_e2e;
pub mod orchestrator;
pub mod recovery;
pub mod repl;
pub mod session_resume;
pub mod tracing_setup;
pub mod version;
pub mod welcome;

pub use cli::{Cli, Command, parse_args};
pub use repl::ReplLoop;
