//! Canonical hook event names.
//!
//! Event names cross a crate boundary: `coderex-core` fires them through the
//! [`HookExecutor`](crate::hook_executor::HookExecutor) trait, and
//! `coderex-hooks` matches them against the names configured in settings.
//! Because matching is an exact string comparison, two different spellings of
//! the same event silently never match — the hook appears configured but never
//! runs, with no error anywhere.
//!
//! These constants are the single source of truth for the spelling. The
//! `coderex-hooks` enum returns them from `HookEvent::as_str`, and a test there
//! asserts the two stay in sync.

/// Before a tool is used.
pub const PRE_TOOL_USE: &str = "PreToolUse";
/// After a tool is used successfully.
pub const POST_TOOL_USE: &str = "PostToolUse";
/// After a tool use fails.
pub const POST_TOOL_USE_FAILURE: &str = "PostToolUseFailure";
/// Before a user prompt is submitted.
pub const USER_PROMPT_SUBMIT: &str = "UserPromptSubmit";
/// Before context compaction.
pub const PRE_COMPACT: &str = "PreCompact";
/// After context compaction.
pub const POST_COMPACT: &str = "PostCompact";
/// At the end of a turn.
pub const TURN_END: &str = "TurnEnd";
/// When the assistant has finished responding and the turn is about to close.
pub const STOP: &str = "Stop";
/// When a tool call is waiting for the person's decision.
pub const PERMISSION_REQUEST: &str = "PermissionRequest";
/// When a tool call was refused.
pub const PERMISSION_DENIED: &str = "PermissionDenied";

#[cfg(test)]
mod tests {
    use super::*;

    /// The names are user-visible: they appear verbatim in `settings.toml`,
    /// so a rename is a breaking change and must be deliberate.
    #[test]
    fn names_are_stable() {
        assert_eq!(PRE_TOOL_USE, "PreToolUse");
        assert_eq!(POST_TOOL_USE, "PostToolUse");
        assert_eq!(POST_TOOL_USE_FAILURE, "PostToolUseFailure");
        assert_eq!(USER_PROMPT_SUBMIT, "UserPromptSubmit");
        assert_eq!(PRE_COMPACT, "PreCompact");
        assert_eq!(POST_COMPACT, "PostCompact");
        assert_eq!(TURN_END, "TurnEnd");
        assert_eq!(STOP, "Stop");
        assert_eq!(PERMISSION_REQUEST, "PermissionRequest");
        assert_eq!(PERMISSION_DENIED, "PermissionDenied");
    }

    #[test]
    fn names_are_unique() {
        let all = [
            PRE_TOOL_USE,
            POST_TOOL_USE,
            POST_TOOL_USE_FAILURE,
            USER_PROMPT_SUBMIT,
            PRE_COMPACT,
            POST_COMPACT,
            TURN_END,
            STOP,
            PERMISSION_REQUEST,
            PERMISSION_DENIED,
        ];
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "hook event names must be distinct");
    }
}
