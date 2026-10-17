//! CLI argument parsing with clap.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    #[arg(long, value_name = "DIR")]
    pub directory: Option<String>,

    /// Additional directories to include in context.
    #[arg(short = 'a', long, value_name = "DIR")]
    pub add_dir: Vec<String>,

    /// Permission mode (default, accept-edits, plan, dont-ask).
    #[arg(long, value_name = "MODE")]
    pub permissions: Option<String>,

    /// Skip permission prompts (dangerous — use with caution).
    #[arg(long, alias = "allow-dangerously-skip-permissions")]
    pub dangerously_skip_permissions: bool,

    /// Comma-separated list of allowed tools.
    #[arg(long, value_name = "TOOLS")]
    pub allowed_tools: Option<String>,

    /// Model to use (e.g., claude-sonnet-4-6, claude-opus-4-6).
    #[arg(short = 'm', long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Fallback model if primary is unavailable.
    #[arg(long, value_name = "MODEL")]
    pub fallback_model: Option<String>,

    /// Custom system prompt.
    #[arg(long, value_name = "PROMPT")]
    pub system_prompt: Option<String>,

    /// Append to the default system prompt instead of replacing it.
    #[arg(long)]
    pub append_system_prompt: bool,

    /// Include a file in context.
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Vec<PathBuf>,

    /// Non-interactive mode: read prompt from stdin, print response, exit.
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Trim surrounding whitespace from headless output, and say nothing
    /// else: this build does not reformat the answer.
    #[arg(long)]
    pub brief: bool,

    /// The same as --brief; kept because scripts pass it.
    #[arg(long)]
    pub bare: bool,

    /// Verbose output (more detail).
    #[arg(long)]
    pub verbose: bool,

    /// Enable debug logging.
    #[arg(short = 'd', long)]
    pub debug: bool,

    /// Write debug output to a file.
    #[arg(long, value_name = "PATH")]
    pub debug_file: Option<PathBuf>,

    /// Continue the most recent session.
    #[arg(short = 'c', long)]
    pub r#continue: bool,

    /// Resume a specific session by ID.
    #[arg(short = 'r', long, value_name = "ID")]
    pub resume: Option<String>,

    /// Session name (used for transcript naming).
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    /// Fork a session from an existing session ID.
    #[arg(long, value_name = "ID")]
    pub fork_session: Option<String>,

    /// Accepted, but this build does not fetch the PR — the flag is recorded
    /// and nothing else happens.
    #[arg(long, value_name = "URL")]
    pub from_pr: Option<String>,

    /// Disable session persistence (do not save transcripts).
    #[arg(long)]
    pub no_session_persistence: bool,

    /// Disable slash commands in the REPL.
    #[arg(long)]
    pub disable_slash_commands: bool,

    /// Maximum budget in USD for this session.
    #[arg(long, value_name = "USD")]
    pub max_budget_usd: Option<f64>,

    /// Maximum number of turns for this session.
    #[arg(long, value_name = "TURNS")]
    pub max_turns: Option<u32>,

    /// API key (overrides environment and config).
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Disable telemetry for this session.
    #[arg(long)]
    pub no_telemetry: bool,

    /// MCP config file path.
    #[arg(long, value_name = "PATH")]
    pub mcp_config: Option<PathBuf>,

    /// Enable MCP debug mode.
    #[arg(long)]
    pub mcp_debug: bool,

    /// Strict MCP config validation (fail on unknown fields).
    #[arg(long)]
    pub strict_mcp_config: bool,

    /// Additional plugin directory to search.
    #[arg(long, value_name = "DIR")]
    pub plugin_dir: Option<PathBuf>,

    /// Settings file path (overrides default ~/.coderex/settings.json).
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,

    /// Show where each setting value comes from.
    #[arg(long)]
    pub setting_sources: bool,

    /// Input format for non-interactive mode (text, json).
    #[arg(long, value_name = "FORMAT", default_value = "text")]
    pub input_format: String,

    /// Output format for non-interactive mode (text, json, markdown, stream-json).
    #[arg(long, value_name = "FORMAT", default_value = "text")]
    pub output_format: String,

    /// JSON schema for structured output (enables JSON mode).
    #[arg(long, value_name = "SCHEMA")]
    pub json_schema: Option<String>,

    /// Replay user messages from a previous session.
    #[arg(long)]
    pub replay_user_messages: bool,

    /// Include partial/incomplete assistant messages in context.
    #[arg(long)]
    pub include_partial_messages: bool,

    /// Set reasoning effort (low, medium, high).
    #[arg(long, value_name = "LEVEL")]
    pub effort: Option<String>,

    /// Accepted, but this build sends no beta headers — the flag is recorded
    /// and nothing else happens.
    #[arg(long)]
    pub betas: Vec<String>,

    /// Enable browser / web integration.
    #[arg(long)]
    pub chrome: bool,

    /// Disable browser / web integration.
    #[arg(long)]
    pub no_chrome: bool,

    /// Accepted, but this build has no tmux integration — the flag is
    /// recorded and nothing else happens.
    #[arg(long)]
    pub tmux: bool,

    /// Accepted, but this build does not switch agents — the flag is
    /// recorded and nothing else happens.
    #[arg(long, value_name = "AGENT")]
    pub agent: Option<String>,

    /// Accepted, but this build has no IDE integration — the flag is
    /// recorded and nothing else happens.
    #[arg(long, value_name = "IDE")]
    pub ide: Option<String>,

    /// Worktree mode (isolated git worktree).
    #[arg(short = 'w', long)]
    pub worktree: bool,

    /// Remaining positional arguments: optional initial prompt.
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the interactive REPL (default when no command is given).
    Repl,
    /// Start in daemon/server mode.
    Serve {
        /// Port to listen on.
        #[arg(short, long, default_value = "8080")]
        port: u16,
    },
    /// Stop a running daemon server.
    ServeStop,
    /// Show status of a running daemon server.
    ServeStatus,
    /// List available agents.
    Agents {
        /// Show detailed agent information.
        #[arg(long)]
        detailed: bool,
    },
    /// Check authentication status.
    Auth,
    /// Run diagnostics and report issues.
    Doctor,
    /// Check for updates.
    Update,
    /// Manage MCP servers.
    Mcp {
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    /// Manage plugins.
    Plugin {
        #[command(subcommand)]
        action: Option<PluginAction>,
    },
    /// Configure auto-mode settings.
    AutoMode,
}

/// MCP management subcommands.
#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// List configured MCP servers.
    List,
    /// Add an MCP server.
    Add { name: String, command: String },
    /// Remove an MCP server.
    Remove { name: String },
    /// Test connection to an MCP server.
    Test { name: String },
}

/// Plugin management subcommands.
#[derive(Subcommand, Debug)]
pub enum PluginAction {
    /// List installed plugins.
    List,
    /// Install a plugin from a marketplace or path.
    Install { source: String },
    /// Uninstall a plugin.
    Uninstall { name: String },
    /// Update all plugins.
    Update,
}

/// Parse CLI arguments from std::env::args.
#[must_use]
pub fn parse_args() -> Cli {
    Cli::parse()
}
