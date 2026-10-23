//! In-process JavaScript extension hooks for CodeRex.
//!
//! A *mod* is a `.js`, `.jsx`, `.ts` or `.tsx` file in a mods directory that
//! declares a global `register(on, options)` function. `.js` files are
//! evaluated as they are; the other extensions are lowered first: TypeScript
//! types are stripped and JSX becomes classic-runtime calls to the global
//! `h`, with `Fragment` for fragments. `h` and `Fragment` build the same
//! descriptors as [`$.ui`](#ui), and a component function is invoked with its
//! props. `on(event, matcher, hook)` subscribes a
//! synchronous `hook($, e, next)` to a hook event. When an event fires, the
//! runtime runs the matching hooks, and merges what they return into a
//! [`HookFireResult`](coderex_core::hook_executor::HookFireResult).
//!
//! A hook may call `next(e)` to continue the chain: the matching hooks
//! registered after it run in order, then the downstream (settings) hook
//! executor, and the combined result is returned to the caller. `next` is
//! single-use per hook invocation; the hook's non-null return fields then
//! override the downstream result, `stop` is OR-ed, and watch paths are
//! deduplicated. A single call may nest at most 64 `next` levels.
//!
//! The host API exposed to a mod is deliberately small: `$.session` (read
//! only facts plus `notify(text)`, a short display-only notice),
//! `$.log(message)` and `$.env(name)` over a fixed allowlist. Mods get no
//! tools, no filesystem, no network and no engine object; `next` only runs the
//! host's existing chain and downstream executor.
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
//! A timed-out worker cannot be interrupted, so it is detached and a later
//! call starts a new one. After [`DEFAULT_TIMEOUT_BREAKER_LIMIT`] such
//! abandonments (overridable with `CODEREX_MODS_TIMEOUT_LIMIT`), the runtime's
//! circuit breaker disables mods for the rest of the session: no worker is
//! started again and a gating `fire` fails closed with an explanatory error.
//! Only wall-clock timeouts count; ordinary hook errors do not. See
//! [`ModRuntime::is_disabled`].
//!
//! [`ModRuntime::load_report`] returns a [`ModInfo`] per mod file with the
//! SHA-256 of its original bytes. The hash is informational: there is no
//! signature or trust store, and mods run because the person installed them.
//!
//! # UI
//!
//! A hook may build a declarative UI tree with `$.ui.text`, `$.ui.box`,
//! `$.ui.button` and stage it with `$.ui.render`, or stage a short notice with
//! `$.session.notify(text)`. Both cross the worker boundary as a host-owned
//! snapshot in
//! [`HookFireResult`](coderex_core::hook_executor::HookFireResult) `ui`, and
//! the engine emits it as `EngineEvent::ModUi` when non-empty. Notices are
//! `UiStyle::Notice` text elements, ordered before the same call's rendered
//! elements. A button's id can be bound with `$.ui.onActivate(id, handler)`,
//! and the terminal routes a focused activation to that handler through
//! [`ModUiHandle`]. Rendering is terminal-only. See `docs/mods.md`.
//!
//! # Not yet
//!
//! There are no async hooks.

mod executor;
mod runtime;
mod transform;

pub use executor::{JsHookExecutor, ModUiHandle};
pub use runtime::{
    ALLOWED_ENV_VARS, DEFAULT_TIMEOUT_BREAKER_LIMIT, MODS_DIR_ENV, MODS_ENV,
    MODS_TIMEOUT_LIMIT_ENV, ModInfo, ModLimits, ModRuntime, ModRuntimeBuilder, ModSession,
    is_enabled, mods_dir,
};
