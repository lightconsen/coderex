//! Agent type definitions.

/// How thoroughly an Explore agent should search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thoroughness {
    /// Quick scan of top-level files and directories.
    Quick,
    /// Moderate depth including subdirectories.
    Medium,
    /// Full recursive search with content analysis.
    VeryThorough,
}

/// The type of sub-agent to spawn.
#[derive(Debug, Clone)]
pub enum AgentType {
    /// General-purpose agent for multi-step tasks.
    GeneralPurpose,
    /// Fast agent specialized for codebase exploration.
    Explore {
        /// How deeply to search.
        thoroughness: Thoroughness,
    },
    /// Agent for designing implementation plans.
    Plan,
    /// Agent for verification tasks.
    Verification,
    /// Agent that answers questions about Claude Code itself.
    ClaudeCodeGuide,
    /// Agent for statusline setup.
    StatuslineSetup,
    /// Custom agent with a user-provided prompt.
    Custom {
        /// The system prompt for this agent.
        prompt: String,
    },
}
