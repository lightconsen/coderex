//! CLI argument parsing with clap.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "coderex", version, about = "CodeRex — AI-powered coding assistant")]
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

#[derive(Subcommand)]
pub enum Command {
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
