//! Hook executor trait — abstract hook firing for QueryEngine.
//!
//! This trait lives in `coderex-core` to avoid a circular dependency with
//! `coderex-hooks`. Concrete implementations (backed by `HookRegistry`)
//! live in `coderex-hooks`.

use std::collections::HashMap;

/// Result of firing a single hook event.
///
/// # Fail-closed contract
///
/// Hooks gate work: a `PreToolUse` hook can deny a tool, a `UserPromptSubmit`
/// hook can refuse a prompt. That gate is only trustworthy if the hook actually
/// answered. A hook that could not be fed its input, whose result could not be
/// retrieved, or which could not answer at all must not be read as "Continue" —
/// that turns a broken hook into a silent allow.
///
/// So when any matching hook fails, [`failed`](Self::failed) is set, the reason
/// is appended to [`errors`](Self::errors), and — on the events where the
/// firing site reads [`stop`](Self::stop) as a gate (`PreToolUse`,
/// `UserPromptSubmit`) — `stop` is set too, so the caller denies. Callers that
/// want to distinguish "the hook said stop" from "the hook could not answer"
/// should read `failed`; callers that only want the gate should read `stop`.
#[derive(Debug, Clone, Default)]
pub struct HookFireResult {
    /// Whether the hook requested to stop the operation.
    ///
    /// Also set when a hook failed on a gating event (fail-closed): a gate we
    /// could not get an answer for is treated as denied. See the type-level
    /// docs.
    pub stop: bool,
    /// Optional replacement for the user input.
    pub updated_input: Option<String>,
    /// Errors encountered during hook execution (empty on success).
    pub errors: Vec<String>,
    /// Whether at least one matching hook failed and produced no usable answer.
    ///
    /// Failure means the hook could not be run, could not be given its input,
    /// produced output that could not be parsed, or timed out. When `failed` is
    /// set on a gating event, [`stop`](Self::stop) is set as well.
    pub failed: bool,
    /// Absolute paths the hook asked the host to watch.
    ///
    /// A `SessionStart` result may carry `watchPaths`; the engine collects
    /// those paths and merges them with the static `watch` configuration so a
    /// `FileChanged` hook can observe files the session only learned about at
    /// start-up. The parser has already rejected relative paths.
    pub watch_paths: Vec<String>,
    /// Replacement for the tool's output, when a `PostToolUse` hook supplied
    /// one.
    ///
    /// The caller substitutes it for what the tool produced before the model
    /// sees the result.
    pub updated_tool_output: Option<String>,
}

/// Abstract hook executor — implemented by `coderex-hooks::HookRegistry`.
#[async_trait::async_trait]
pub trait HookExecutor: Send + Sync {
    /// Fire a hook event and return the result.
    ///
    /// `event_name` is the string name of the event (e.g. "UserPromptSubmit").
    /// `input` is the current input string (user prompt, tool name, etc.).
    /// `context` is additional context (session_id, tool args, etc.).
    ///
    /// Implementations must fail closed: if a hook cannot answer, the returned
    /// [`HookFireResult`] must not read as an allow. See [`HookFireResult`].
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult;

    /// Absolute paths the installed hooks statically asked to watch.
    ///
    /// This is the `watch` field on `FileChanged` hook configuration. An
    /// executor that has no such concept (the no-op executor, a test double)
    /// has nothing to watch and keeps the default.
    fn watch_paths(&self) -> Vec<String> {
        Vec::new()
    }
}

/// No-op hook executor — used when hooks are not configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpHookExecutor;

#[async_trait::async_trait]
impl HookExecutor for NoOpHookExecutor {
    async fn fire_hook(
        &self,
        _event_name: &str,
        _input: &str,
        _context: &HashMap<String, String>,
    ) -> HookFireResult {
        HookFireResult::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The no-op executor is not a failed hook — it simply has nothing to say.
    /// Conflating the two would make every hook-less session look broken.
    #[tokio::test]
    async fn noop_executor_is_not_a_failure() {
        let result = NoOpHookExecutor
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;
        assert!(!result.failed);
        assert!(!result.stop);
        assert!(result.errors.is_empty());
    }
}
