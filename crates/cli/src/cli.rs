//! CLI argument parsing with clap.

use clap::{Args, Parser, Subcommand};
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

    /// Append text to the default system prompt.
    ///
    /// Independent of `--system-prompt`: `--system-prompt` replaces the base
    /// prompt, while this text is always additive. Both may be given together.
    #[arg(long, value_name = "TEXT")]
    pub append_system_prompt: Option<String>,

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

    /// Fetch a pull request's title, description, files, commits, and diff
    /// with the GitHub CLI and prepend them to the initial prompt.
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

    /// Comma-separated feature flags: `+name` enables, `-name` disables, a
    /// bare `name` enables. Applied on top of CODEREX_FEATURE_FLAGS.
    #[arg(long, value_name = "FLAGS")]
    pub betas: Vec<String>,

    /// Enable browser / web integration.
    #[arg(long)]
    pub chrome: bool,

    /// Disable browser / web integration.
    #[arg(long)]
    pub no_chrome: bool,

    /// Run inside tmux: re-launch in a new session when outside one, or set
    /// the window/pane title when already inside.
    #[arg(long)]
    pub tmux: bool,

    /// Run the session as a named custom agent.
    ///
    /// The name refers to a JSON definition in `~/.coderex/agents/`; its
    /// system prompt and tool whitelist are applied to the session. An
    /// unknown name is an error listing the configured agents.
    #[arg(long, value_name = "AGENT")]
    pub agent: Option<String>,

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
#[allow(clippy::large_enum_variant)] // BridgeArgs carries the whole launch configuration.
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
    /// Run the bridge gateway: control plane, work submission and frame relay.
    Gateway {
        /// Port to listen on.
        #[arg(long, default_value = "8788")]
        port: u16,
    },
    /// Run the teleport rendezvous service (session sharing between machines).
    TeleportService {
        /// Port to listen on.
        #[arg(long, default_value = "8787")]
        port: u16,
        /// How long a share stays readable, in seconds (default one week).
        #[arg(long, default_value = "604800")]
        ttl_secs: u64,
    },
    /// Show status of a running daemon server.
    ServeStatus,
    /// List available agents (built-in and custom).
    Agents {
        /// Show detailed agent information.
        #[arg(long)]
        detailed: bool,
    },
    /// Check authentication status.
    Auth,
    /// Run diagnostics and report issues.
    Doctor,
    /// Type-check the exported plugin declarations with a TypeScript compiler.
    PluginTypecheck {
        /// Directory holding the exported declarations and `tsconfig.json`.
        /// Defaults to `<config home>/plugin-types`.
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// Install the latest release, or report whether one exists.
    Update {
        /// Report whether a newer release exists without downloading it.
        #[arg(long)]
        check: bool,
    },
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
    /// Inspect telemetry locally without uploading anything.
    Telemetry {
        #[command(subcommand)]
        action: Option<TelemetryAction>,
    },
    /// Run the bridge worker that serves the remote relay transport.
    Bridge(BridgeArgs),
}

/// Telemetry inspection subcommands.
///
/// These read the same local state the uploader would read and print the wire
/// payload, but they never send it anywhere.
#[derive(Subcommand, Debug)]
pub enum TelemetryAction {
    /// Print the JSON that would be uploaded for a pending crash.
    ///
    /// Reads the newest `*.json` file in the pending crash directory, or the
    /// file given as an argument. `CODEREX_CRASH_VERBOSE` and `CODEREX_HIPAA`
    /// affect the payload exactly as they do for a real upload.
    PreviewCrash {
        /// A pending crash file to preview (defaults to the newest one).
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },
}

/// Arguments for `coderex bridge`.
///
/// The control-plane URL and environment ID identify this worker to the
/// gateway; every other value has a default and can be overridden by flag or
/// environment variable.
#[derive(Args, Debug, Clone)]
pub struct BridgeArgs {
    /// Control-plane base URL (HTTP API).
    #[arg(
        long,
        value_name = "URL",
        env = "CODEREX_BRIDGE_CONTROL_PLANE_URL",
        required = true
    )]
    pub control_plane_url: Option<String>,

    /// Environment identifier registered with the control plane.
    #[arg(
        long,
        value_name = "ID",
        env = "CODEREX_BRIDGE_ENV_ID",
        required = true
    )]
    pub env_id: Option<String>,

    /// Git repository root (defaults to the current directory).
    #[arg(long, value_name = "DIR", env = "CODEREX_BRIDGE_REPO_ROOT")]
    pub repo_root: Option<PathBuf>,

    /// Base directory for session worktrees (defaults to
    /// `<repo>/.coderex/worktrees`).
    #[arg(long, value_name = "DIR", env = "CODEREX_BRIDGE_WORKTREE_BASE")]
    pub worktree_base: Option<PathBuf>,

    /// Base directory for session data (defaults to
    /// `<repo>/.coderex/sessions`).
    #[arg(long, value_name = "DIR", env = "CODEREX_BRIDGE_SESSION_BASE")]
    pub session_base: Option<PathBuf>,

    /// Base directory for archived sessions (defaults to
    /// `<repo>/.coderex/archives`).
    #[arg(long, value_name = "DIR", env = "CODEREX_BRIDGE_ARCHIVE_BASE")]
    pub archive_base: Option<PathBuf>,

    /// Maximum concurrent sessions.
    #[arg(
        long,
        value_name = "N",
        env = "CODEREX_BRIDGE_MAX_CONCURRENT",
        default_value_t = 5
    )]
    pub max_concurrent: usize,

    /// Transport kind: `websocket` (alias `ws`) or `in-process`.
    #[arg(
        long,
        value_name = "KIND",
        env = "CODEREX_BRIDGE_TRANSPORT",
        default_value = "websocket"
    )]
    pub transport: String,

    /// Transport endpoint. Defaults to the control-plane URL with the
    /// `http`/`https` scheme rewritten to `ws`/`wss`.
    ///
    /// `CODEREX_BRIDGE_URL` keeps the historical `ws://host:port` spelling.
    #[arg(long, value_name = "URL", env = "CODEREX_BRIDGE_URL")]
    pub transport_url: Option<String>,

    /// Connection identifier reported by the transport (defaults to the
    /// environment ID).
    #[arg(long, value_name = "ID", env = "CODEREX_BRIDGE_CONNECTION_ID")]
    pub connection_id: Option<String>,

    /// Bearer token sent in the transport `Authorization` header.
    #[arg(
        long,
        value_name = "TOKEN",
        env = "CODEREX_BRIDGE_TOKEN",
        hide_env_values = true
    )]
    pub auth_token: Option<String>,

    /// Heartbeat interval in seconds.
    #[arg(
        long,
        value_name = "SECS",
        env = "CODEREX_BRIDGE_HEARTBEAT_INTERVAL_SECS",
        default_value_t = 30
    )]
    pub heartbeat_interval_secs: u64,

    /// Heartbeat timeout in seconds.
    #[arg(
        long,
        value_name = "SECS",
        env = "CODEREX_BRIDGE_HEARTBEAT_TIMEOUT_SECS",
        default_value_t = 10
    )]
    pub heartbeat_timeout_secs: u64,

    /// Consecutive missed heartbeats before the status is degraded.
    #[arg(
        long,
        value_name = "N",
        env = "CODEREX_BRIDGE_HEARTBEAT_DEGRADED_THRESHOLD",
        default_value_t = 2
    )]
    pub heartbeat_degraded_threshold: usize,

    /// Consecutive missed heartbeats before the status is lost.
    #[arg(
        long,
        value_name = "N",
        env = "CODEREX_BRIDGE_HEARTBEAT_LOST_THRESHOLD",
        default_value_t = 5
    )]
    pub heartbeat_lost_threshold: usize,

    /// Interval between control-plane assignment polls, in seconds.
    #[arg(
        long,
        value_name = "SECS",
        env = "CODEREX_BRIDGE_POLL_INTERVAL_SECS",
        default_value_t = 5
    )]
    pub poll_interval_secs: u64,

    /// Path template for claiming an assignment before running it.
    ///
    /// `{env_id}` and `{assignment_id}` are substituted. The default matches
    /// the bundled control-plane server; override it for a deployment with a
    /// different claim route.
    #[arg(
        long,
        value_name = "PATH",
        env = "CODEREX_BRIDGE_CLAIM_PATH",
        default_value = "/api/environments/{env_id}/assignments/{assignment_id}/claim"
    )]
    pub claim_path: Option<String>,
}

/// MCP management subcommands.
#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// List configured MCP servers.
    List,
    /// Add an MCP server (stdio command or remote URL).
    Add {
        /// Name to register the server under.
        name: String,
        /// Command to run for a stdio server (mutually exclusive with `--url`).
        #[arg(value_name = "COMMAND", conflicts_with = "url")]
        command: Option<String>,
        /// Endpoint for a remote server.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Transport for a remote server: `http` (default), `streamable-http`, or `sse`.
        #[arg(long, value_name = "TRANSPORT")]
        transport: Option<String>,
        /// Extra HTTP header for a remote server, as `KEY=VALUE`. Repeatable.
        #[arg(long = "header", value_name = "KEY=VALUE")]
        header: Vec<String>,
    },
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

/// The hidden argument that makes this binary act as the WASM compiler.
///
/// Shared with the plugins crate so the injected command and the handler can
/// never disagree about the spelling.
pub const INTERNAL_WASM_COMPILE_FLAG: &str =
    coderex_plugins::wasm_runtime::INTERNAL_WASM_COMPILE_FLAG;

/// Run the hidden WASM compiler subcommand when it is the first argument.
///
/// This runs *before* clap sees the arguments, so
/// `coderex --internal-wasm-compile <input> <output>` cannot be mistaken for a
/// user-facing flag. Returns `Some(exit_code)` when the command was handled.
#[must_use]
pub fn maybe_run_internal_wasm_compile() -> Option<i32> {
    let mut args = std::env::args_os();
    let _program = args.next()?;
    if args.next().as_deref() != Some(std::ffi::OsStr::new(INTERNAL_WASM_COMPILE_FLAG)) {
        return None;
    }

    Some(
        match coderex_plugins::wasm_runtime::compile_helper_main(args) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("coderex: {err}");
                1
            }
        },
    )
}

/// Point the WASM runtime at this executable as its compiler.
///
/// The runtime appends `<input> <output>` to the injected command, so a
/// packaged `coderex` binary needs no sibling `coderex-wasm-compile`.
///
/// # Errors
///
/// Returns the platform error when the current executable's path cannot be
/// determined.
pub fn install_wasm_compiler() -> std::io::Result<()> {
    let program = std::env::current_exe()?;
    coderex_plugins::wasm_runtime::set_compiler_command(
        coderex_plugins::wasm_runtime::WasmCompilerCommand::new(program)
            .arg(INTERNAL_WASM_COMPILE_FLAG),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_file(cli: Cli) -> Option<PathBuf> {
        match cli.command {
            Some(Command::Telemetry {
                action: Some(TelemetryAction::PreviewCrash { file }),
            }) => file,
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn telemetry_preview_crash_defaults_to_the_newest_pending_file() {
        let cli = Cli::try_parse_from(["coderex", "telemetry", "preview-crash"]).unwrap();
        assert_eq!(preview_file(cli), None);
    }

    #[test]
    fn telemetry_preview_crash_accepts_an_explicit_file() {
        let cli =
            Cli::try_parse_from(["coderex", "telemetry", "preview-crash", "/tmp/crash_1.json"])
                .unwrap();
        assert_eq!(preview_file(cli), Some(PathBuf::from("/tmp/crash_1.json")));
    }

    #[test]
    fn telemetry_without_an_action_is_still_parseable() {
        let cli = Cli::try_parse_from(["coderex", "telemetry"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Telemetry { action: None })
        ));
    }

    #[test]
    fn append_system_prompt_takes_a_value() {
        let cli = Cli::try_parse_from(["coderex", "--append-system-prompt", "be terse"]).unwrap();
        assert_eq!(cli.append_system_prompt.as_deref(), Some("be terse"));
        assert!(cli.system_prompt.is_none());
    }

    #[test]
    fn append_system_prompt_without_a_value_is_a_parse_error() {
        assert!(Cli::try_parse_from(["coderex", "--append-system-prompt"]).is_err());
    }

    #[test]
    fn system_prompt_and_append_system_prompt_compose() {
        let cli = Cli::try_parse_from([
            "coderex",
            "--system-prompt",
            "replace the base",
            "--append-system-prompt",
            "add this",
        ])
        .unwrap();
        assert_eq!(cli.system_prompt.as_deref(), Some("replace the base"));
        assert_eq!(cli.append_system_prompt.as_deref(), Some("add this"));
    }

    #[test]
    fn append_system_prompt_is_independent_of_the_replacement_prompt() {
        let cli =
            Cli::try_parse_from(["coderex", "--append-system-prompt", "only additive"]).unwrap();
        assert!(cli.system_prompt.is_none());
        assert_eq!(cli.append_system_prompt.as_deref(), Some("only additive"));
    }
}
