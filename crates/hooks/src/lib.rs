//! CodeRex hook system — 22 events, sync/async execution, configuration.

#![warn(missing_docs)]

pub mod action;
pub mod engine;
pub mod events;
mod sandbox;
pub mod stop_hook;
mod stream;

pub use action::{
    AGENT_DEFAULT_MODEL, AGENT_DEFAULT_TIMEOUT_MS, ActionPrompt, HookActionRunner, McpToolHook,
    PROMPT_DEFAULT_TIMEOUT_MS, interpolate_tool_input, substitute_arguments,
};
pub use engine::{HookConfig, HookInput, HookMatcher, HookRegistry, HookTier, HookType, HttpHook};
pub use events::{
    HookAction, HookError, HookEvent, HookPermissionDecision, HookResult, UnknownHookEvent,
};
pub use stop_hook::{
    StopHookContext, StopHookDecision, execute_stop_hook, should_proceed_with_compact,
};
pub use stream::{
    HookHostRequest, HookRequestError, HookRequestHandler, HookStreamEvent, METHOD_NOT_FOUND,
};
