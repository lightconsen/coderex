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
    ///
    /// Fired once by `coderex_core::engine` before the session's first turn.
    SessionStart,
    /// During initial setup.
    ///
    /// Fired by the REPL when the `/init` slash command succeeds — the
    /// product's setup action. The registry dispatch is synchronous, so the
    /// REPL queues the command in `sync_command_state` and fires this hook
    /// from its async entry points (`execute_command` and the TUI loop) via
    /// [`QueryEngine::fire_lifecycle_hook`](coderex_core::engine::QueryEngine::fire_lifecycle_hook).
    Setup,
    /// When a sub-agent starts.
    ///
    /// Fired by `coderex_agents::manager::AgentManager::spawn_agent` once the
    /// sub-agent is created and about to run.
    SubagentStart,
    /// When a notification is emitted.
    ///
    /// Fired by `coderex_core::engine` when the agent surfaces an advisory
    /// notification to the person, such as a provider outage.
    Notification,
    /// When permission is requested.
    PermissionRequest,
    /// When permission is denied.
    PermissionDenied,
    /// When the working directory changes.
    ///
    /// Fired by `coderex_tools::RegistryToolDispatcher::set_cwd` when the
    /// session's working directory moves (for example into a worktree).
    CwdChanged,
    /// When a file changes.
    ///
    /// Fired by the REPL's TUI loop when `coderex_config::ConfigWatcher`
    /// reports a change to the user or project settings file. The hook input
    /// and its `paths` context entry name the changed paths. The watcher is
    /// drained only by the long-lived TUI loop: a headless one-shot run has
    /// no loop to select on it, so this event does not fire there.
    FileChanged,
    /// When a worktree is created.
    ///
    /// Fired by the `EnterWorktree` tool after the worktree is created.
    WorktreeCreate,
    /// Before context compaction.
    PreCompact,
    /// After context compaction.
    PostCompact,
    /// When the user requests a stop.
    Stop,
    /// When a sub-agent stops.
    ///
    /// Fired by `coderex_agents::manager::AgentManager` when a sub-agent
    /// finishes (or is stopped / completed) — the counterpart to
    /// [`SubagentStart`](Self::SubagentStart).
    SubagentStop,
    /// When a task is completed.
    ///
    /// Fired by the `TaskUpdate` tool when a task's status becomes
    /// `completed`. This is the task-store lifecycle, not the sub-agent
    /// teammate event of the same name.
    TaskCompleted,
    /// When a teammate goes idle.
    ///
    /// Fired by `coderex_agents::manager::AgentManager` where a teammate
    /// reports idleness — after a turn completes, and via `mark_idle`.
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
    /// This is the single source of truth for event names. Registry matching
    /// canonicalizes both the fired name and the configured name through
    /// [`FromStr`](std::str::FromStr), so the historical `PostToolUseFail`
    /// alias still matches. Call sites should build the name from here rather
    /// than writing the literal.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            // The events core fires are named by core's `hook_events`
            // constants, so the firing side and the matching side cannot
            // drift apart.
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
    /// Whether the firing site reads a `stop` decision on this event as a gate.
    ///
    /// On `PreToolUse` a `stop` denies the tool; on `UserPromptSubmit` it
    /// refuses the prompt. Those are the only events where the caller consults
    /// [`HookFireResult::stop`](coderex_core::hook_executor::HookFireResult::stop),
    /// so they are the only events where a hook that *cannot answer* has a
    /// fail-closed response available (deny). Elsewhere a failed hook can only
    /// be recorded, not turned into a denial.
    ///
    /// Keep this in sync with the firing sites: a new caller that gates on
    /// `stop` must add its event here, or a failed hook there will read as an
    /// allow.
    #[must_use]
    pub const fn is_gating(self) -> bool {
        matches!(self, Self::PreToolUse | Self::UserPromptSubmit)
    }
}

impl HookEvent {
    /// Whether this event carries a tool call, so its hook input names a tool.
    ///
    /// These are the events the scheduler fires around a single tool call: the
    /// hook input's `input` field is the tool's name and its `context` field is
    /// the tool's arguments. A hook's `if` condition is a *tool rule* on
    /// exactly these events; on any other event there is no tool to match, so
    /// the rule can only decline (see
    /// [`HookCondition`](crate::engine::HookCondition)).
    #[must_use]
    pub const fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::PermissionRequest
                | Self::PermissionDenied
        )
    }
}

impl HookEvent {
    /// Whether CodeRex fires this event anywhere.
    ///
    /// The firing sites, so this list can be checked against the code:
    ///
    /// - `UserPromptSubmit`, `SessionStart`, `TurnEnd`, `Stop`, `PreCompact`,
    ///   `PostCompact`, `Notification`, `Elicitation`, `ElicitationResult` —
    ///   `coderex_core::engine`
    /// - `PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
    ///   `PermissionRequest`, `PermissionDenied` —
    ///   `coderex_core::tool_scheduler`
    /// - `SubagentStart`, `SubagentStop`, `TeammateIdle` —
    ///   `coderex_agents::manager`
    /// - `WorktreeCreate` — the `EnterWorktree` tool
    /// - `TaskCompleted` — the `TaskUpdate` tool
    /// - `CwdChanged` — `coderex_tools::RegistryToolDispatcher::set_cwd`
    /// - `Setup` — the REPL, when `/init` succeeds
    /// - `FileChanged` — the REPL's TUI loop, when `ConfigWatcher` reports a
    ///   settings-file change (not the headless one-shot path)
    ///
    /// Every name CodeRex accepts is fired somewhere; there are no reserved
    /// names left. A test pins the list so adding a variant without a firing
    /// site is a deliberate change rather than a silently inert hook.
    #[must_use]
    pub const fn is_triggered(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::UserPromptSubmit
                | Self::SessionStart
                | Self::Setup
                | Self::Notification
                | Self::PermissionRequest
                | Self::PermissionDenied
                | Self::CwdChanged
                | Self::FileChanged
                | Self::PreCompact
                | Self::PostCompact
                | Self::Stop
                | Self::Elicitation
                | Self::ElicitationResult
                | Self::TurnEnd
                | Self::SubagentStart
                | Self::SubagentStop
                | Self::TeammateIdle
                | Self::WorktreeCreate
                | Self::TaskCompleted
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
    /// Replacement for the tool's output (`PostToolUse`).
    ///
    /// The reference's `updatedToolOutput`: a hook that inspects a tool result
    /// can rewrite what the model sees, so an auditing hook can redact,
    /// reformat, or annotate it instead of only being able to stop the turn.
    pub updated_tool_output: Option<String>,
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
    /// The hook's input could not be delivered to it.
    ///
    /// The hook ran — or started to — without its input, so its answer cannot
    /// be trusted. Fail closed: see the `HookFireResult` contract.
    #[error("hook input could not be delivered: {0}")]
    InputNotDelivered(String),
    /// The hook produced no output, so it gave no answer.
    ///
    /// An empty stdout is not "continue": a hook that cannot answer is a failed
    /// hook, and a failed hook must not be read as an allow. Hooks that want to
    /// continue without a decision must say so explicitly with `{}` or
    /// `{"action":"continue"}`.
    #[error("hook produced no output (expected a JSON result on stdout)")]
    NoOutput,
    /// The hook's output could not be parsed as a hook result.
    #[error("hook output could not be parsed: {0}")]
    UnparseableOutput(String),
    /// The hook produced more output than the streaming protocol accepts.
    ///
    /// The limit exists so a hook cannot exhaust the host's memory: a pipe
    /// gives the writer backpressure, but only if the reader bounds what it
    /// keeps.
    #[error("hook output exceeded the {0}-byte limit")]
    OutputTooLarge(usize),
    /// Invalid configuration for an `http` hook.
    #[error("invalid http hook: {0}")]
    InvalidHttpHook(String),
    /// The hook's HTTP request did not complete.
    #[error("hook request failed: {0}")]
    RequestFailed(String),
    /// The hook's endpoint answered with a status that is not a success.
    ///
    /// A redirect counts here: the body is the conversation, so following one
    /// would send it to a destination the configuration did not name.
    #[error("hook endpoint answered {0}")]
    HttpStatus(u16),
    /// A `prompt`, `agent`, or `mcp_tool` hook ran without a
    /// [`HookActionRunner`](crate::HookActionRunner) installed.
    ///
    /// Those kinds need a model client or the MCP registry, neither of which
    /// belongs in this crate, so the session that owns them injects a runner
    /// with [`HookRegistry::set_action_runner`](crate::HookRegistry::set_action_runner).
    /// Running without one is a **failure**, not a skip: hooks gate work, and a
    /// gate that cannot run must deny on a gating event.
    #[error(
        "this session cannot run {kind} hooks: no action runner is installed. The session \
         that owns the model client or the MCP registry must call \
         HookRegistry::set_action_runner before installing the registry"
    )]
    MissingActionRunner {
        /// The hook kind that could not run (`prompt`, `agent`, `mcp_tool`).
        kind: &'static str,
    },
    /// Invalid configuration for a `prompt` or `agent` hook.
    #[error("invalid prompt hook: {0}")]
    InvalidPromptHook(String),
    /// Invalid configuration for an `mcp_tool` hook.
    #[error("invalid mcp_tool hook: {0}")]
    InvalidMcpToolHook(String),
    /// A `${tool_input.<path>}` reference in an `mcp_tool` hook's input could
    /// not be resolved.
    ///
    /// A reference that cannot be resolved is a failure rather than an empty
    /// substitution, so a hook cannot silently call a tool with a missing
    /// argument.
    #[error("hook tool input could not be interpolated: {0}")]
    InterpolationFailed(String),
    /// The session's own client could not carry out the hook's action.
    ///
    /// `prompt`, `agent` and `mcp_tool` run through the client the session
    /// installed rather than through a subprocess, so their failures are that
    /// client's — and a client that could not answer is a hook that did not
    /// answer.
    #[error("hook action failed: {0}")]
    ActionFailed(String),
    /// The OS sandbox could not be created for a hook that needs it.
    ///
    /// A gating hook is refused rather than run unconfined: a configured gate
    /// that cannot be enforced must not read as an allow. Non-gating hooks
    /// log the reason and run with resource limits only, so this error is the
    /// gating path.
    #[error("hook OS sandbox is unavailable: {0}")]
    SandboxUnavailable(String),
    /// A hook's `sandbox` configuration cannot be honoured.
    ///
    /// Refused at load time so a typo or a relative path is visible instead of
    /// silently confining nothing.
    #[error("invalid hook sandbox configuration: {0}")]
    InvalidSandboxConfig(String),
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
                "SessionStart",
                "Setup",
                "SubagentStart",
                "Notification",
                "PermissionRequest",
                "PermissionDenied",
                "CwdChanged",
                "FileChanged",
                "WorktreeCreate",
                "PreCompact",
                "PostCompact",
                "Stop",
                "SubagentStop",
                "TaskCompleted",
                "TeammateIdle",
                "Elicitation",
                "ElicitationResult",
                "TurnEnd",
            ]
        );
    }

    /// Nothing is reserved: every name CodeRex accepts has a firing site.
    /// Adding a variant without one must update this test deliberately rather
    /// than shipping an inert hook name.
    #[test]
    fn no_event_is_reserved() {
        let untriggered: Vec<&str> = HookEvent::iter()
            .filter(|event| !event.is_triggered())
            .map(HookEvent::as_str)
            .collect();

        assert!(
            untriggered.is_empty(),
            "these events are accepted but never fire: {untriggered:?}"
        );
    }

    /// Only the events the scheduler fires around one tool call carry a tool
    /// name and its arguments.
    #[test]
    fn only_the_tool_events_carry_a_tool() {
        let tool_events: Vec<&str> = HookEvent::iter()
            .filter(|event| event.is_tool_event())
            .map(HookEvent::as_str)
            .collect();

        assert_eq!(
            tool_events,
            vec![
                "PreToolUse",
                "PostToolUse",
                "PostToolUseFailure",
                "PermissionRequest",
                "PermissionDenied",
            ]
        );
        for event in HookEvent::iter().filter(|e| e.is_tool_event()) {
            assert!(
                event.is_triggered(),
                "{event:?} carries a tool but never fires"
            );
        }
    }

    /// Only the events whose firing site reads `stop` as a gate may report as
    /// gating. A failed hook on a non-gating event has nowhere to deny.
    #[test]
    fn only_the_gating_events_report_as_gating() {
        let gating: Vec<&str> = HookEvent::iter()
            .filter(|event| event.is_gating())
            .map(HookEvent::as_str)
            .collect();
        assert_eq!(gating, vec!["PreToolUse", "UserPromptSubmit"]);
    }

    /// A gating event must also be one CodeRex actually fires — otherwise the
    /// gate is a promise about a hook that can never run.
    #[test]
    fn every_gating_event_is_triggered() {
        for event in HookEvent::iter().filter(|e| e.is_gating()) {
            assert!(event.is_triggered(), "{event:?} gates but never fires");
        }
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
            HookEvent::SessionStart,
            HookEvent::Notification,
            HookEvent::PermissionRequest,
            HookEvent::PermissionDenied,
            HookEvent::PreCompact,
            HookEvent::PostCompact,
            HookEvent::Stop,
            HookEvent::Elicitation,
            HookEvent::ElicitationResult,
            HookEvent::TurnEnd,
        ];
        let expected = [
            core_events::PRE_TOOL_USE,
            core_events::POST_TOOL_USE,
            core_events::POST_TOOL_USE_FAILURE,
            core_events::USER_PROMPT_SUBMIT,
            core_events::SESSION_START,
            core_events::NOTIFICATION,
            core_events::PERMISSION_REQUEST,
            core_events::PERMISSION_DENIED,
            core_events::PRE_COMPACT,
            core_events::POST_COMPACT,
            core_events::STOP,
            core_events::ELICITATION,
            core_events::ELICITATION_RESULT,
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
