//! Hook executor trait — abstract hook firing for QueryEngine.
//!
//! This trait lives in `coderex-core` to avoid a circular dependency with
//! `coderex-hooks`. Concrete implementations (backed by `HookRegistry`)
//! live in `coderex-hooks`.

use std::collections::HashMap;

/// Result of firing a single hook event.
#[derive(Debug, Clone, Default)]
pub struct HookFireResult {
    /// Whether the hook requested to stop the operation.
    pub stop: bool,
    /// Optional replacement for the user input.
    pub updated_input: Option<String>,
    /// Errors encountered during hook execution (empty on success).
    pub errors: Vec<String>,
}

/// Abstract hook executor — implemented by `coderex-hooks::HookRegistry`.
#[async_trait::async_trait]
pub trait HookExecutor: Send + Sync {
    /// Fire a hook event and return the result.
    ///
    /// `event_name` is the string name of the event (e.g. "UserPromptSubmit").
    /// `input` is the current input string (user prompt, tool name, etc.).
    /// `context` is additional context (session_id, tool args, etc.).
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult;
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
