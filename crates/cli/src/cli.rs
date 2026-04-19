//! CLI argument parsing with clap.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "coderex",
    version,
    about = "CodeRex — AI-powered coding assistant",
    long_about = "CodeRex — AI-powered coding assistant.\n\
\n\
Running without a subcommand starts the interactive REPL."
)]
pub struct Cli {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub directory: Option<String>,

    /// Permission mode.
    #[arg(long)]
    pub permissions: Option<String>,

    /// Model to use.
    #[arg(long)]
    pub model: Option<String>,

    /// Non-interactive mode: read prompt from stdin.
    #[arg(long)]
    pub print: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the interactive REPL (default when no command is given).
    Repl,
    /// Start in daemon/server mode.
    Serve,
    /// Resume a previous session.
    Resume { session_id: String },
}

/// Parse CLI arguments from std::env::args.
#[must_use]
pub fn parse_args() -> Cli {
    Cli::parse()
}
