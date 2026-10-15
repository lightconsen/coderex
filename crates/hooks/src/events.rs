//! Hook event definitions and result types.

use serde::{Deserialize, Serialize};

/// The hook events CodeRex knows the names of.
///
/// A name here is not a promise that anything fires it. The events CodeRex
/// actually fires are listed by [`HookEvent::is_triggered`], and every other
/// variant is a reserved name: settings may use it, and
/// [`HookRegistry`](crate::HookRegistry) reports it as inert on load rather
/// than accepting a hook that can never run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum HookEvent {
    /// Before a tool is used.
    PreToolUse,
    /// After a tool is used.
    PostToolUse,
    /// After a tool use fails.
    PostToolUseFailure,
    /// Before a user prompt is submitted.
    UserPromptSubmit,
    /// When a session starts.
    SessionStart,
    /// During initial setup.
    Setup,
    /// When a sub-agent starts.
    SubagentStart,
    /// When a notification is emitted.
    Notification,
    /// When permission is requested.
    PermissionRequest,
    /// When permission is denied.
    PermissionDenied,
    /// When the working directory changes.
    CwdChanged,
    /// When a file changes.
    FileChanged,
    /// When a worktree is created.
    WorktreeCreate,
    /// Before context compaction.
    PreCompact,
    /// After context compaction.
    PostCompact,
    /// When the user requests a stop.
    Stop,
    /// When a sub-agent stops.
    SubagentStop,
    /// When a task is completed.
    TaskCompleted,
    /// When a teammate goes idle.
    TeammateIdle,
    /// When an elicitation is requested.
    Elicitation,
    /// When an elicitation result is received.
    ElicitationResult,
    /// At the end of a turn.
    TurnEnd,
}

impl HookEvent {
    /// The canonical wire name for this event.
    ///
    /// This is the single source of truth for event names: registry matching
    /// compares the fired name against the name configured in settings, so any
    /// two spellings of the same event silently never match. Call sites should
    /// build the name from here rather than writing the literal.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            // The seven events core actually fires are named by core's
            // `hook_events` constants, so the firing side and the matching
            // side cannot drift apart.
            Self::PreToolUse => coderex_core::hook_events::PRE_TOOL_USE,
            Self::PostToolUse => coderex_core::hook_events::POST_TOOL_USE,
            Self::PostToolUseFailure => coderex_core::hook_events::POST_TOOL_USE_FAILURE,
            Self::UserPromptSubmit => coderex_core::hook_events::USER_PROMPT_SUBMIT,
            Self::SessionStart => "SessionStart",
            Self::Setup => "Setup",
            Self::SubagentStart => "SubagentStart",
            Self::Notification => "Notification",
            Self::PermissionRequest => coderex_core::hook_events::PERMISSION_REQUEST,
            Self::PermissionDenied => coderex_core::hook_events::PERMISSION_DENIED,
            Self::CwdChanged => "CwdChanged",
            Self::FileChanged => "FileChanged",
            Self::WorktreeCreate => "WorktreeCreate",
            Self::PreCompact => coderex_core::hook_events::PRE_COMPACT,
            Self::PostCompact => coderex_core::hook_events::POST_COMPACT,
            Self::Stop => coderex_core::hook_events::STOP,
            Self::SubagentStop => "SubagentStop",
            Self::TaskCompleted => "TaskCompleted",
            Self::TeammateIdle => "TeammateIdle",
            Self::Elicitation => "Elicitation",
            Self::ElicitationResult => "ElicitationResult",
            Self::TurnEnd => coderex_core::hook_events::TURN_END,
        }
    }
}

impl HookEvent {
    /// Whether CodeRex fires this event anywhere.
    ///
    /// The firing sites, so this list can be checked against the code:
    ///
    /// - `UserPromptSubmit`, `TurnEnd`, `Stop`, `PreCompact`, `PostCompact` —
    ///   `coderex_core::engine`
    /// - `PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
    ///   `PermissionRequest`, `PermissionDenied` —
    ///   `coderex_core::tool_scheduler`
    ///
    /// Everything else is a name CodeRex accepts but never fires. A hook
    /// configured on one of those would install and then never run, so
    /// [`HookRegistry`](crate::HookRegistry) warns about it at load.
    #[must_use]
    pub const fn is_triggered(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::UserPromptSubmit
                | Self::PermissionRequest
                | Self::PermissionDenied
                | Self::PreCompact
                | Self::PostCompact
                | Self::Stop
                | Self::TurnEnd
        )
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for HookEvent {
    type Err = UnknownHookEvent;

    /// Parse a configured event name.
    ///
    /// Accepts the canonical name and the historical `PostToolUseFail` alias,
    /// so settings written against the old spelling keep working.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let event = match s {
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "PostToolUseFailure" | "PostToolUseFail" => Self::PostToolUseFailure,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "SessionStart" => Self::SessionStart,
            "Setup" => Self::Setup,
            "SubagentStart" => Self::SubagentStart,
            "Notification" => Self::Notification,
            "PermissionRequest" => Self::PermissionRequest,
            "PermissionDenied" => Self::PermissionDenied,
            "CwdChanged" => Self::CwdChanged,
            "FileChanged" => Self::FileChanged,
            "WorktreeCreate" => Self::WorktreeCreate,
            "PreCompact" => Self::PreCompact,
            "PostCompact" => Self::PostCompact,
            "Stop" => Self::Stop,
            "SubagentStop" => Self::SubagentStop,
            "TaskCompleted" => Self::TaskCompleted,
            "TeammateIdle" => Self::TeammateIdle,
            "Elicitation" => Self::Elicitation,
            "ElicitationResult" => Self::ElicitationResult,
            "TurnEnd" => Self::TurnEnd,
            _ => return Err(UnknownHookEvent(s.to_string())),
        };
        Ok(event)
    }
}

/// A hook event name that does not correspond to any known event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown hook event: {0}")]
pub struct UnknownHookEvent(pub String);

/// The result of a hook execution.
#[derive(Debug, Clone)]
pub struct HookResult {
    /// Whether to continue or stop the operation.
    pub action: HookAction,
    /// Whether to suppress the normal output.
    pub suppress_output: bool,
    /// Updated input to replace the original input.
    pub updated_input: Option<String>,
    /// Additional context to append.
    pub additional_context: Option<String>,
    /// File paths to watch (for file watcher hooks).
    pub watch_paths: Vec<String>,
    /// Permission decision (for PermissionRequest hooks).
    pub permission_decision: Option<HookPermissionDecision>,
}

/// What the hook decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Continue with the operation.
    Continue,
    /// Stop the operation.
    Stop,
}

/// Permission decision from a hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookPermissionDecision {
    /// Whether to allow the action.
    pub allow: bool,
    /// If allow is true, optionally restrict to a specific path.
    pub allow_for_path: Option<String>,
    /// Reason for the decision.
    pub reason: String,
}

/// Hook execution errors.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// Hook timed out.
    #[error("hook timed out after {0}ms")]
    Timeout(u64),
    /// Failed to spawn the hook process.
    #[error("failed to spawn hook process: {0}")]
    SpawnFailed(String),
    /// Configuration not found.
    #[error("hook configuration not found")]
    ConfigNotFound,
    /// Invalid command in hook configuration.
    #[error("invalid hook command: {0}")]
    InvalidCommand(String),
    /// JSON parse/serialize error.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    /// Hook execution failed (non-zero exit).
    #[error("hook exited with code {0}")]
    ExecutionFailed(i32),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use strum::IntoEnumIterator;

    /// The events CodeRex fires, and only those. Anything else in the enum is
    /// a name CodeRex knows but never fires, which a configured hook has to be
    /// warned about.
    #[test]
    fn only_the_wired_events_report_as_triggered() {
        let triggered: Vec<&str> = HookEvent::iter()
            .filter(|event| event.is_triggered())
            .map(HookEvent::as_str)
            .collect();

        assert_eq!(
            triggered,
            vec![
                "PreToolUse",
                "PostToolUse",
                "PostToolUseFailure",
                "UserPromptSubmit",
                "PermissionRequest",
                "PermissionDenied",
                "PreCompact",
                "PostCompact",
                "Stop",
                "TurnEnd",
            ]
        );
    }

    #[test]
    fn event_names_are_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for event in HookEvent::iter() {
            let name = event.as_str();
            assert!(!name.is_empty(), "{event:?} has an empty name");
            assert!(seen.insert(name), "duplicate event name: {name}");
        }
    }

    /// Every event core fires must be nameable here, with the same spelling.
    /// A mismatch means the hook is configured but never runs.
    #[test]
    fn core_fired_events_match_enum_names() {
        use coderex_core::hook_events as core_events;

        let fired = [
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::PostToolUseFailure,
            HookEvent::UserPromptSubmit,
            HookEvent::PreCompact,
            HookEvent::PostCompact,
            HookEvent::TurnEnd,
        ];
        let expected = [
            core_events::PRE_TOOL_USE,
            core_events::POST_TOOL_USE,
            core_events::POST_TOOL_USE_FAILURE,
            core_events::USER_PROMPT_SUBMIT,
            core_events::PRE_COMPACT,
            core_events::POST_COMPACT,
            core_events::TURN_END,
        ];
        for (event, name) in fired.iter().zip(expected) {
            assert_eq!(event.as_str(), name);
        }
    }

    #[test]
    fn round_trips_through_string() {
        for event in HookEvent::iter() {
            assert_eq!(HookEvent::from_str(event.as_str()), Ok(event));
        }
    }

    /// Settings written against the pre-rename spelling must keep matching.
    #[test]
    fn accepts_legacy_alias() {
        assert_eq!(
            HookEvent::from_str("PostToolUseFail"),
            Ok(HookEvent::PostToolUseFailure)
        );
    }

    #[test]
    fn rejects_unknown_names() {
        assert!(HookEvent::from_str("NoSuchEvent").is_err());
    }
}
