//! Session and application state definitions.

use crate::types::{Message, ModelId, UsageSummary};
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use uuid::Uuid;

/// State of a single conversation session.
#[derive(Debug, Clone)]
pub struct SessionState {
    /// Unique session identifier.
    pub session_id: Uuid,
    /// User-facing or AI-generated title.
    pub title: Option<String>,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Token usage summary.
    pub usage: UsageSummary,
    /// Cumulative cost in USD.
    pub cost_usd: f64,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last active.
    pub last_active_at: DateTime<Utc>,
    /// Current working directory.
    pub cwd: PathBuf,
    /// Active git worktree, if any.
    pub active_worktree: Option<WorktreeState>,
}

impl SessionState {
    /// Create a new session for the given working directory.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(cwd: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            session_id: Uuid::new_v4(),
            title: None,
            messages: Vec::new(),
            usage: UsageSummary {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            cost_usd: 0.0,
            created_at: now,
            last_active_at: now,
            cwd,
            active_worktree: None,
        }
    }
}

/// State of a git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeState {
    /// Path to the worktree directory.
    pub path: PathBuf,
    /// Branch or commit the worktree is based on.
    pub base: String,
    /// Whether the worktree has uncommitted changes.
    pub has_changes: bool,
}

/// Application-wide state.
///
/// Concrete subsystem state (settings, tasks, MCP, plugins, agents, UI,
/// todos, notifications, coordinator, bridge) lives in dedicated crates
/// (`coderex-config`, `coderex-agents`, `coderex-mcp`, `coderex-plugins`,
/// `coderex-bridge`, etc.). Those subsystems are held by the CLI/REPL
/// runtime, not here, to keep `coderex-core` free of circular dependencies.
/// This struct only tracks engine-level global state that is independent
/// of any subsystem crate.
#[derive(Debug)]
pub struct AppState {
    /// Current model being used.
    pub main_loop_model: ModelId,
}

impl AppState {
    /// Create a new AppState with defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            main_loop_model: ModelId::sonnet(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ─── SessionState constructor tests (Phase 0.3 requirement) ────────────

    #[test]
    fn session_state_new_has_correct_fields() {
        let cwd = PathBuf::from("/tmp/test");
        let state = SessionState::new(cwd.clone());

        assert_eq!(state.cwd, cwd);
        assert!(state.title.is_none());
        assert!(state.messages.is_empty());
        assert_eq!(state.usage.input_tokens, 0);
        assert_eq!(state.usage.output_tokens, 0);
        assert_eq!(state.usage.cache_read_tokens, 0);
        assert_eq!(state.usage.cache_creation_tokens, 0);
        assert_eq!(state.cost_usd, 0.0);
        assert!(state.active_worktree.is_none());
        // Timestamps should be very recent
        let now = Utc::now();
        assert!((state.created_at - now).num_seconds().abs() < 1);
        assert!((state.last_active_at - now).num_seconds().abs() < 1);
    }

    #[test]
    fn session_state_has_unique_ids() {
        let s1 = SessionState::new(PathBuf::from("/a"));
        let s2 = SessionState::new(PathBuf::from("/b"));
        assert_ne!(s1.session_id, s2.session_id);
    }

    #[test]
    fn session_state_new_has_empty_messages() {
        let state = SessionState::new(PathBuf::from("/test"));
        assert!(state.messages.is_empty());
    }

    #[test]
    fn app_state_new_has_default_model() {
        let state = AppState::new();
        assert_eq!(state.main_loop_model, ModelId::sonnet());
    }
}
