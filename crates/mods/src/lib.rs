//! In-process JavaScript extension hooks for CodeRex.
//!
//! A *mod* is a `.js` file in a mods directory that declares a global
//! `register(on, options)` function. `on(event, matcher, hook)` subscribes a
//! synchronous `hook($, e, next)` to a hook event. When an event fires, the
//! runtime runs the matching hooks, and merges what they return into a
//! [`HookFireResult`](coderex_core::hook_executor::HookFireResult).
//!
//! A hook may call `next(e)` to run the downstream (settings) hook chain and
//! receive its result. `next` is single-use per hook invocation; the hook's
//! non-null return fields then override the downstream result, `stop` is
//! OR-ed, and watch paths are deduplicated.
//!
//! The host API exposed to a mod is deliberately small: `$.session` (read
//! only), `$.log(message)` and `$.env(name)` over a fixed allowlist. Mods get
//! no tools, no filesystem, no network and no engine object; `next` only
//! invokes the host's existing downstream executor.
//!
//! The runtime is opt-in. It is constructed only when [`is_enabled`] is true,
//! which means `CODEREX_MODS` is truthy or `CODEREX_MODS_DIR` is set. The
//! directory defaults to `config_home()/mods`.
//!
//! # Isolation
//!
//! A `boa_engine` [`Context`](boa_engine::Context) is neither `Send` nor
//! interruptible from another thread, and `next` must resume in the context
//! that called it. Each [`ModRuntime`] therefore owns one long-lived worker
//! thread with one context and one realm per mod. [`ModRuntime::fire`] sends a
//! request over a channel and waits under a wall-clock budget that covers the
//! whole chain, including any downstream run. boa's loop-iteration, recursion
//! and stack limits bound a runaway mod. A mod that loops forever is reported
//! as a hook error rather than hanging the agent. See [`ModLimits`] for the
//! exact ceilings.
//!
//! # Not yet
//!
//! The reference runtime also lets mods contribute UI elements. That needs an
//! element tree and a renderer, neither of which exists here. See
//! `docs/mods.md` for the design.

mod executor;
mod runtime;

pub use executor::JsHookExecutor;
pub use runtime::{
    ALLOWED_ENV_VARS, MODS_DIR_ENV, MODS_ENV, ModLimits, ModRuntime, ModRuntimeBuilder, ModSession,
    is_enabled, mods_dir,
};
