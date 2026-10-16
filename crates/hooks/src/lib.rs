//! CodeRex hook system — 22 events, sync/async execution, configuration.

#![warn(missing_docs)]

pub mod engine;
pub mod events;
pub mod stop_hook;

pub use engine::{HookConfig, HookInput, HookMatcher, HookRegistry, HookTier, HookType, HttpHook};
pub use events::{
    HookAction, HookError, HookEvent, HookPermissionDecision, HookResult, UnknownHookEvent,
};
pub use stop_hook::{
    StopHookContext, StopHookDecision, execute_stop_hook, should_proceed_with_compact,
};
