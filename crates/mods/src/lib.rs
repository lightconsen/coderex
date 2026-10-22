//! In-process JavaScript extension hooks for CodeRex.
//!
//! A *mod* is a `.js` file in a mods directory that declares a global
//! `register(on, options)` function. `on(event, matcher, hook)` subscribes a
//! synchronous function to a hook event. When an event fires, the runtime
//! re-creates a JavaScript realm on a dedicated thread, runs the matching
//! hooks, and merges what they return into a
//! [`HookFireResult`](coderex_core::hook_executor::HookFireResult).
//!
//! The host API exposed to a mod is deliberately small: `$.session` (read
//! only), `$.log(message)` and `$.env(name)` over a fixed allowlist. Mods get
//! no tools, no filesystem, no network and no engine object.
//!
//! The runtime is opt-in. It is constructed only when [`is_enabled`] is true,
//! which means `CODEREX_MODS` is truthy or `CODEREX_MODS_DIR` is set. The
//! directory defaults to `config_home()/mods`.
//!
//! # Isolation
//!
//! A `boa_engine` [`Context`](boa_engine::Context) is neither `Send` nor
//! interruptible from another thread. Each hook call therefore runs on its own
//! thread with its own realm, a wall-clock budget enforced by the calling
//! thread, and boa's loop-iteration, recursion and stack limits. A mod that
//! loops forever is reported as a hook error rather than hanging the agent.
//! See [`ModLimits`] for the exact ceilings.
//!
//! # Not yet
//!
//! The reference runtime threads a `next(e)` continuation through a chain of
//! hooks, and lets mods contribute UI elements. Neither exists here: a hook is
//! a single synchronous call with a return value. See `docs/mods.md` for the
//! design those two features need.

mod executor;
mod runtime;

pub use executor::JsHookExecutor;
pub use runtime::{
    ALLOWED_ENV_VARS, MODS_DIR_ENV, MODS_ENV, ModLimits, ModRuntime, ModRuntimeBuilder, ModSession,
    is_enabled, mods_dir,
};
