//! Command registry — manages Local, LocalUi, Prompt, and Standalone commands.

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

/// A registered slash command.
#[derive(Debug)]
pub struct SlashCommand {
    /// The command name (e.g., "clear" for /clear).
    pub name: &'static str,
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
}

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
}
