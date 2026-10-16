//! Command registry — manages Local, LocalUi, Prompt, and Standalone commands.
//!
//! Phase 12.1: Defines `CommandType`, `SlashCommand`, `CommandResult`,
//! `CommandContext`, and `CommandRegistry` with availability matrix.

use coderex_core::permissions::PermissionMode;
use coderex_core::query_profiles::PromptProfile;
use coderex_core::types::ModelId;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

// ─── CommandType ────────────────────────────────────────────────────────────

/// The type of a slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandType {
    /// Runs locally, output is inline.
    Local,
    /// Runs locally with UI components (overlays, modals).
    LocalUi,
    /// Expands to a prompt sent to the model.
    Prompt,
    /// Runs as a standalone subprocess.
    Standalone,
}

// ─── CommandResult ──────────────────────────────────────────────────────────

/// Result of executing a slash command.
#[derive(Debug, Clone, Serialize)]
pub struct CommandResult {
    /// Text output to display to the user.
    pub output: String,
    /// Whether the command changed session state.
    pub state_changed: bool,
    /// Whether to suppress the output from the user.
    pub suppress_output: bool,
    /// Additional context to inject into the session.
    pub extra_context: Option<String>,
    /// System prompt to append for mode switching.
    pub append_system_prompt: Option<String>,
    /// System prompt to fully override the default base prompt.
    pub override_system_prompt: Option<String>,
    /// Prompt profile to switch the agent behavior.
    pub prompt_profile: Option<PromptProfile>,
    /// Override allowed tool names for this session.
    pub allowed_tools_override: Option<Vec<String>>,
    /// Whether the output should be rendered as a scrollable message.
    pub scrollable: bool,
}

impl CommandResult {
    /// Create a simple success result with the given output.
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            state_changed: false,
            suppress_output: false,
            extra_context: None,
            append_system_prompt: None,
            override_system_prompt: None,
            prompt_profile: None,
            allowed_tools_override: None,
            scrollable: false,
        }
    }

    /// Create an error result.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            output: format!("Error: {}", message.into()),
            state_changed: false,
            suppress_output: false,
            extra_context: None,
            append_system_prompt: None,
            override_system_prompt: None,
            prompt_profile: None,
            allowed_tools_override: None,
            scrollable: false,
        }
    }

    /// Mark the result as having changed session state.
    pub fn with_state_change(mut self, changed: bool) -> Self {
        self.state_changed = changed;
        self
    }

    /// Suppress the output display.
    pub fn with_suppressed(mut self) -> Self {
        self.suppress_output = true;
        self
    }

    /// Add extra context to the session.
    pub fn with_context(mut self, ctx: impl Into<String>) -> Self {
        self.extra_context = Some(ctx.into());
        self
    }

    /// Set a system prompt to append (for mode switching).
    pub fn with_append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.append_system_prompt = Some(prompt.into());
        self
    }

    /// Override the base system prompt entirely.
    ///
    /// When set, this replaces the default base system prompt instead of
    /// appending to it. This is useful for commands that want to switch
    /// the agent into a completely different behavior mode.
    pub fn with_override_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.override_system_prompt = Some(prompt.into());
        self
    }

    /// Set the prompt profile to switch agent behavior.
    pub fn with_prompt_profile(mut self, profile: PromptProfile) -> Self {
        self.prompt_profile = Some(profile);
        self
    }

    /// Override the allowed tool set for this session.
    pub fn with_allowed_tools_override(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools_override = Some(tools);
        self
    }

    /// Mark the output as scrollable (rendered as a full message).
    pub fn with_scrollable(mut self) -> Self {
        self.scrollable = true;
        self
    }
}

/// Command argument specification for validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandArgs {
    /// No specific argument validation — any number of arguments accepted.
    None,
    /// At least N arguments required.
    AtLeast(usize),
    /// At most N arguments allowed.
    AtMost(usize),
    /// Exactly N arguments required.
    Exactly(usize),
}

impl Default for CommandArgs {
    fn default() -> Self {
        CommandArgs::None
    }
}

// ─── CommandHandler ─────────────────────────────────────────────────────────

/// A command handler function.
pub type CommandHandler = Arc<dyn Fn(&CommandContext, &[String]) -> CommandResult + Send + Sync>;

// ─── CommandContext ─────────────────────────────────────────────────────────

/// Runtime context provided to command handlers.
#[derive(Clone, Debug)]
pub struct CommandContext {
    /// Current working directory.
    pub cwd: String,
    /// Current model ID.
    pub model: ModelId,
    /// Current permission mode.
    pub permission_mode: PermissionMode,
    /// Message count in the current session.
    pub message_count: usize,
    /// Total input tokens used.
    pub input_tokens: usize,
    /// Total output tokens used.
    pub output_tokens: usize,
    /// Total cache read tokens.
    pub cache_read_tokens: usize,
    /// Total cache creation tokens.
    pub cache_creation_tokens: usize,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Session ID.
    pub session_id: String,
    /// Session title.
    pub session_title: String,
    /// Elapsed time in seconds.
    pub elapsed_secs: u64,
    /// Whether a plan file exists.
    pub has_plan: bool,
    /// Whether MCP is connected.
    pub mcp_connected: bool,
    /// Active hooks.
    pub active_hooks: usize,
    /// API key configured.
    pub api_key_set: bool,
    /// Feature flags (key -> enabled).
    pub feature_flags: HashMap<String, bool>,
    /// Git branch name.
    pub git_branch: String,
    /// Git status summary.
    pub git_status: String,
    /// Theme name.
    pub theme: String,
    /// Whether vim mode is enabled.
    pub vim_mode: bool,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Version string.
    pub version: String,
    /// Pre-formatted cost summary from the cost tracker.
    pub cost_summary: String,
    /// Pipe input from a previous command in a chain (e.g., `/diff | /commit`).
    pub pipe_input: Option<String>,
}

impl CommandContext {
    /// Create a new command context.
    #[must_use]
    pub fn new(cwd: String, session_id: String, session_title: String, model: ModelId) -> Self {
        Self {
            cwd,
            model,
            permission_mode: PermissionMode::default(),
            message_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: 0.0,
            session_id,
            session_title,
            elapsed_secs: 0,
            has_plan: false,
            mcp_connected: false,
            active_hooks: 0,
            api_key_set: false,
            feature_flags: HashMap::new(),
            git_branch: String::new(),
            git_status: String::new(),
            theme: String::new(),
            vim_mode: false,
            env: HashMap::new(),
            version: String::new(),
            cost_summary: String::new(),
            pipe_input: None,
        }
    }

    /// Check if a feature flag is enabled.
    #[must_use]
    pub fn is_feature_enabled(&self, flag: &str) -> bool {
        self.feature_flags.get(flag).copied().unwrap_or(false)
    }
}

// ─── SlashCommand ───────────────────────────────────────────────────────────

/// A registered slash command.
pub struct SlashCommand {
    /// The command name (e.g., "clear" for /clear).
    pub name: &'static str,
    /// Brief description for /help.
    pub description: &'static str,
    /// The command type.
    pub kind: CommandType,
    /// Whether this command works in non-interactive mode.
    pub supports_non_interactive: bool,
    /// Whether this command is safe for remote clients.
    pub remote_safe: bool,
    /// Whether this command is safe for bridge sessions.
    pub bridge_safe: bool,
    /// Override allowed tools when this command runs.
    pub allowed_tools_override: Option<Vec<String>>,
    /// Whether this command should be executed via a sub-agent backend (10.5).
    pub uses_agent_backend: bool,
    /// Whether this command is destructive and requires confirmation.
    pub destructive: bool,
    /// Argument specification for validation.
    pub args: CommandArgs,
    /// The handler function.
    pub handler: CommandHandler,
}

impl std::fmt::Debug for SlashCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlashCommand")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("kind", &self.kind)
            .field("supports_non_interactive", &self.supports_non_interactive)
            .field("remote_safe", &self.remote_safe)
            .field("bridge_safe", &self.bridge_safe)
            .field("allowed_tools_override", &self.allowed_tools_override)
            .field("uses_agent_backend", &self.uses_agent_backend)
            .field("destructive", &self.destructive)
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

impl SlashCommand {
    /// Create a new Local command.
    pub const fn local(
        name: &'static str,
        description: &'static str,
        supports_non_interactive: bool,
        remote_safe: bool,
        bridge_safe: bool,
        handler: CommandHandler,
    ) -> Self {
        Self {
            name,
            description,
            kind: CommandType::Local,
            supports_non_interactive,
            remote_safe,
            bridge_safe,
            allowed_tools_override: None,
            uses_agent_backend: false,
            destructive: false,
            args: CommandArgs::None,
            handler,
        }
    }

    /// Create a new LocalUi command.
    pub const fn local_ui(
        name: &'static str,
        description: &'static str,
        supports_non_interactive: bool,
        handler: CommandHandler,
    ) -> Self {
        Self {
            name,
            description,
            kind: CommandType::LocalUi,
            supports_non_interactive,
            remote_safe: true,
            bridge_safe: true,
            allowed_tools_override: None,
            uses_agent_backend: false,
            destructive: false,
            args: CommandArgs::None,
            handler,
        }
    }

    /// Create a new Prompt command.
    pub const fn prompt(
        name: &'static str,
        description: &'static str,
        supports_non_interactive: bool,
        handler: CommandHandler,
    ) -> Self {
        Self {
            name,
            description,
            kind: CommandType::Prompt,
            supports_non_interactive,
            remote_safe: true,
            bridge_safe: true,
            allowed_tools_override: None,
            uses_agent_backend: false,
            destructive: false,
            args: CommandArgs::None,
            handler,
        }
    }

    /// Create a new Standalone command.
    pub const fn standalone(
        name: &'static str,
        description: &'static str,
        handler: CommandHandler,
    ) -> Self {
        Self {
            name,
            description,
            kind: CommandType::Standalone,
            supports_non_interactive: true,
            remote_safe: false,
            bridge_safe: false,
            allowed_tools_override: None,
            uses_agent_backend: false,
            destructive: false,
            args: CommandArgs::None,
            handler,
        }
    }

    /// Mark this command to be executed via a sub-agent backend.
    pub const fn with_agent_backend(mut self) -> Self {
        self.uses_agent_backend = true;
        self
    }

    /// Mark this command as destructive, requiring confirmation in TUI mode.
    pub const fn with_destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Set the argument validation spec for this command.
    pub const fn with_args(mut self, args: CommandArgs) -> Self {
        self.args = args;
        self
    }
}

// ─── CommandRegistry ────────────────────────────────────────────────────────

/// Registry of all available slash commands.
#[derive(Debug)]
pub struct CommandRegistry {
    commands: Vec<SlashCommand>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate command arguments against the spec.
/// Returns an error result if validation fails, or None if valid.
fn validate_args(spec: &CommandArgs, args: &[String], name: &str) -> Option<CommandResult> {
    match spec {
        CommandArgs::None => {
            // No validation — accept any number of arguments
            None
        }
        CommandArgs::AtLeast(n) => {
            if args.len() < *n {
                Some(CommandResult::error(format!(
                    "Command /{name} requires at least {n} argument(s). Got {}.",
                    args.len()
                )))
            } else {
                None
            }
        }
        CommandArgs::AtMost(n) => {
            if args.len() > *n {
                Some(CommandResult::error(format!(
                    "Command /{name} takes at most {n} argument(s). Got {}.",
                    args.len()
                )))
            } else {
                None
            }
        }
        CommandArgs::Exactly(n) => {
            if args.len() != *n {
                Some(CommandResult::error(format!(
                    "Command /{name} requires exactly {n} argument(s). Got {}.",
                    args.len()
                )))
            } else {
                None
            }
        }
    }
}

impl CommandRegistry {
    /// Create a new empty command registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Register a new command.
    pub fn register(&mut self, command: SlashCommand) {
        self.commands.push(command);
    }

    /// Find a command by name.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&SlashCommand> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// List all commands.
    #[must_use]
    pub fn list(&self) -> &[SlashCommand] {
        &self.commands
    }

    /// Execute a command by name with the given arguments.
    ///
    /// Validates argument count before dispatching to the handler.
    /// Returns a `CommandResult` or an error message if the command is not found.
    pub fn execute(&self, name: &str, ctx: &CommandContext, args: &[String]) -> CommandResult {
        match self.find(name) {
            Some(cmd) => {
                // Validate arguments
                if let Some(err) = validate_args(&cmd.args, args, name) {
                    return err;
                }
                (cmd.handler)(ctx, args)
            }
            None => CommandResult::error(format!("Unknown command: /{name}")),
        }
    }

    /// List commands available in the given context.
    ///
    /// Filters by non-interactive, remote-safe, or bridge-safe availability.
    #[must_use]
    pub fn available_commands(
        &self,
        non_interactive: bool,
        remote_safe: bool,
        bridge_safe: bool,
    ) -> Vec<&SlashCommand> {
        self.commands
            .iter()
            .filter(|c| {
                if non_interactive && !c.supports_non_interactive {
                    return false;
                }
                if remote_safe && !c.remote_safe {
                    return false;
                }
                if bridge_safe && !c.bridge_safe {
                    return false;
                }
                true
            })
            .collect()
    }

    /// Count registered commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Check if no commands are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Remove all registered commands.
    pub fn clear(&mut self) {
        self.commands.clear();
    }
}

// ─── Register All Commands ──────────────────────────────────────────────────

/// Register all built-in commands into the registry.
pub fn register_all_commands(registry: &mut CommandRegistry) {
    // 12.2 Session Management
    super::session_commands::register(registry);
    // 12.3 Config
    super::config_commands::register(registry);
    // 12.4 Stats
    super::stats_commands::register(registry);
    // 12.5 Content & Integration
    super::content_commands::register(registry);
    // 12.6 Session Extension
    super::session_extension_commands::register(registry);
    // 12.7 Config Extension
    super::config_extension_commands::register(registry);
    // 12.8 Stats & Diagnostic
    super::diagnostic_commands::register(registry);
    // 12.9 Content & Context
    super::context_commands::register(registry);
    // 12.10 Integration
    super::integration_commands::register(registry);
    // 12.11 Git/PR
    super::git_commands::register(registry);
    // 12.12 Debug/Diagnostic
    super::debug_commands::register(registry);
    // 12.13 Advanced
    super::advanced_commands::register(registry);
    // 12.14 Miscellaneous
    super::misc_commands::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command's argument spec is enforced before its handler runs: the
    /// declaration existed and `execute` consulted it, and nothing asserted
    /// that the seam works.
    #[test]
    fn a_command_with_too_few_arguments_is_refused_before_it_runs() {
        let mut registry = CommandRegistry::new();
        registry.register(
            SlashCommand::local(
                "needs-two",
                "needs two arguments",
                false,
                false,
                false,
                Arc::new(|_ctx: &CommandContext, args: &[String]| {
                    CommandResult::success(format!("ran with {} args", args.len()))
                }),
            )
            .with_args(CommandArgs::AtLeast(2)),
        );

        let too_few = registry.execute("needs-two", &make_ctx(), &["one".into()]);
        assert!(too_few.output.contains("at least 2"), "{}", too_few.output);
        assert!(
            !too_few.output.contains("ran with"),
            "the handler ran anyway: {}",
            too_few.output
        );

        let enough = registry.execute("needs-two", &make_ctx(), &["one".into(), "two".into()]);
        assert!(enough.output.contains("ran with 2"), "{}", enough.output);
    }

    #[test]
    fn a_command_with_too_many_arguments_is_refused() {
        let mut registry = CommandRegistry::new();
        registry.register(
            SlashCommand::local(
                "takes-one",
                "takes one argument",
                false,
                false,
                false,
                Arc::new(|_ctx: &CommandContext, _args: &[String]| CommandResult::success("ran")),
            )
            .with_args(CommandArgs::AtMost(1)),
        );

        let too_many = registry.execute("takes-one", &make_ctx(), &["a".into(), "b".into()]);
        assert!(too_many.output.contains("at most 1"), "{}", too_many.output);
        assert!(!too_many.output.contains("ran"), "{}", too_many.output);
    }

    #[test]
    fn a_command_with_the_wrong_count_is_refused() {
        let mut registry = CommandRegistry::new();
        registry.register(
            SlashCommand::local(
                "takes-exactly-two",
                "takes exactly two",
                false,
                false,
                false,
                Arc::new(|_ctx: &CommandContext, _args: &[String]| CommandResult::success("ran")),
            )
            .with_args(CommandArgs::Exactly(2)),
        );

        for args in [vec![], vec!["a".into()], vec!["a".into(), "b".into(), "c".into()]] {
            let result = registry.execute("takes-exactly-two", &make_ctx(), &args);
            assert!(
                result.output.contains("exactly 2"),
                "{args:?} gave {}",
                result.output
            );
        }

        let right = registry.execute(
            "takes-exactly-two",
            &make_ctx(),
            &["a".into(), "b".into()],
        );
        assert!(right.output.contains("ran"), "{}", right.output);
    }

    /// `CommandArgs::None` — the default — accepts anything, so a command that
    /// says nothing about its arguments is not accidentally constrained.
    #[test]
    fn a_command_that_says_nothing_about_its_arguments_accepts_them() {
        let mut registry = CommandRegistry::new();
        registry.register(SlashCommand::local(
            "anything",
            "takes anything",
            false,
            false,
            false,
            Arc::new(|_ctx: &CommandContext, args: &[String]| {
                CommandResult::success(format!("ran with {} args", args.len()))
            }),
        ));

        let result = registry.execute("anything", &make_ctx(), &["a".into(), "b".into(), "c".into()]);
        assert!(result.output.contains("ran with 3"), "{}", result.output);
    }

    /// A command marked destructive says so on the command itself, which is
    /// what the REPL asks before running it. The flag has to survive the
    /// registry, or the prompt never happens.
    #[test]
    fn a_destructive_command_carries_the_flag_the_repl_asks_about() {
        let mut registry = CommandRegistry::new();
        registry.register(
            SlashCommand::local(
                "wipe",
                "wipes something",
                false,
                false,
                false,
                Arc::new(|_ctx: &CommandContext, _args: &[String]| CommandResult::success("done")),
            )
            .with_destructive(),
        );
        registry.register(SlashCommand::local(
            "harmless",
            "does nothing much",
            false,
            false,
            false,
            Arc::new(|_ctx: &CommandContext, _args: &[String]| CommandResult::success("done")),
        ));

        assert!(registry.find("wipe").unwrap().destructive);
        assert!(!registry.find("harmless").unwrap().destructive);
    }

    fn make_ctx() -> CommandContext {
        CommandContext::new(
            "/src".into(),
            "sess_1".into(),
            "Test Session".into(),
            ModelId::sonnet(),
        )
    }

    #[test]
    fn registry_starts_empty() {
        let reg = CommandRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn register_and_find_command() {
        let mut reg = CommandRegistry::new();
        reg.register(SlashCommand::local(
            "test",
            "Test command",
            true,
            true,
            true,
            Arc::new(|_, _| CommandResult::success("ok")),
        ));

        let found = reg.find("test");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "test");
    }

    #[test]
    fn find_nonexistent_returns_none() {
        let reg = CommandRegistry::new();
        assert!(reg.find("nope").is_none());
    }

    #[test]
    fn execute_known_command() {
        let mut reg = CommandRegistry::new();
        reg.register(SlashCommand::local(
            "hello",
            "Say hello",
            true,
            true,
            true,
            Arc::new(|_, _| CommandResult::success("Hello!")),
        ));

        let ctx = make_ctx();
        let result = reg.execute("hello", &ctx, &[]);
        assert_eq!(result.output, "Hello!");
    }

    #[test]
    fn execute_unknown_command() {
        let reg = CommandRegistry::new();
        let ctx = make_ctx();
        let result = reg.execute("nope", &ctx, &[]);
        assert!(result.output.contains("Unknown command"));
    }

    #[test]
    fn command_result_helpers() {
        let r = CommandResult::success("test");
        assert!(!r.state_changed);
        assert!(!r.suppress_output);

        let r = CommandResult::success("test").with_state_change(true);
        assert!(r.state_changed);

        let r = CommandResult::success("test").with_suppressed();
        assert!(r.suppress_output);

        let r = CommandResult::success("test").with_context("extra info");
        assert_eq!(r.extra_context, Some("extra info".into()));

        let e = CommandResult::error("oops");
        assert!(e.output.contains("Error"));
    }

    #[test]
    fn availability_matrix_filters() {
        let mut reg = CommandRegistry::new();
        reg.register(SlashCommand::local(
            "safe",
            "safe",
            true,
            true,
            true,
            Arc::new(|_, _| CommandResult::success("ok")),
        ));
        reg.register(SlashCommand::local(
            "unsafe",
            "unsafe",
            true,
            false,
            false,
            Arc::new(|_, _| CommandResult::success("ok")),
        ));
        reg.register(SlashCommand::local(
            "interactive",
            "interactive",
            false,
            true,
            true,
            Arc::new(|_, _| CommandResult::success("ok")),
        ));

        // Remote-safe only
        let remote = reg.available_commands(false, true, false);
        assert_eq!(remote.len(), 2);
        assert!(remote.iter().any(|c| c.name == "safe"));
        assert!(remote.iter().any(|c| c.name == "interactive"));

        // Non-interactive only
        let non_interactive = reg.available_commands(true, false, false);
        assert_eq!(non_interactive.len(), 2);
        assert!(non_interactive.iter().any(|c| c.name == "safe"));
        assert!(non_interactive.iter().any(|c| c.name == "unsafe"));

        // Bridge-safe only
        let bridge = reg.available_commands(false, false, true);
        assert_eq!(bridge.len(), 2);
        assert!(bridge.iter().any(|c| c.name == "safe"));
        assert!(bridge.iter().any(|c| c.name == "interactive"));
    }

    #[test]
    fn list_returns_all() {
        let mut reg = CommandRegistry::new();
        reg.register(SlashCommand::local(
            "a",
            "a",
            true,
            true,
            true,
            Arc::new(|_, _| CommandResult::success("ok")),
        ));
        reg.register(SlashCommand::local(
            "b",
            "b",
            true,
            true,
            true,
            Arc::new(|_, _| CommandResult::success("ok")),
        ));
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn command_result_serializes() {
        let r = CommandResult::success("hello").with_state_change(true);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("hello"));
        assert!(json.contains("state_changed"));
    }

    #[test]
    fn with_agent_backend() {
        let cmd = SlashCommand::local(
            "test",
            "test",
            true,
            true,
            true,
            Arc::new(|_, _| CommandResult::success("ok")),
        );
        assert!(!cmd.uses_agent_backend);

        let cmd = cmd.with_agent_backend();
        assert!(cmd.uses_agent_backend);
    }

    #[test]
    fn context_feature_flags() {
        let mut flags = HashMap::new();
        flags.insert("test_flag".into(), true);
        let ctx = CommandContext {
            feature_flags: flags,
            ..make_ctx()
        };
        assert!(ctx.is_feature_enabled("test_flag"));
        assert!(!ctx.is_feature_enabled("nonexistent"));
    }
}
