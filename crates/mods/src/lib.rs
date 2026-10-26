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
//! only facts plus `notify(text)`, a short display-only notice, and
//! `submit(text)`, which queues a user message for after the current turn),
//! `$.log(message)` and `$.env(name)` over a fixed allowlist. Mods get no
//! tools, no filesystem, no network and no engine object; `next` only runs the
//! host's existing chain and downstream executor.
//!
//! `$.session.submit(text)` never calls the engine. It stages text on the
//! worker; the chain merges what matching hooks submitted (deduplicating
//! identical consecutive messages) into
//! [`HookFireResult::pending_prompts`](coderex_core::hook_executor::HookFireResult::pending_prompts),
//! and the runtime also pushes them onto a [`ModSubmitQueue`]. The engine
//! ignores the field; the turn-loop owner (the CLI) drains the queue when the
//! turn is idle and submits each message as an ordinary user turn, through
//! the same permission pipeline as typed input. Caps: two submissions per
//! hook call, eight queued messages per turn. Overflow is dropped, counted,
//! and reported in the hook result's UI.
//!
//! The runtime is opt-in. It is constructed only when [`is_enabled`] is true,
//! which means `CODEREX_MODS` is truthy or `CODEREX_MODS_DIR` is set. The
//! directory defaults to `config_home()/mods`.
//!
//! # Isolation
//!
//! A QuickJS runtime and its contexts are confined to one thread, and `next`
//! must resume in the realm that called it. Each [`ModRuntime`] therefore owns
//! one long-lived worker thread with one runtime and one context (realm) per
//! mod. [`ModRuntime::fire`] sends a request over a channel and waits under a
//! wall-clock budget that covers the whole chain, including any downstream
//! run.
//!
//! The engine enforces the limits itself: a heap cap (`set_memory_limit`), a
//! maximum stack (`set_max_stack_size`) and an interrupt handler driven by the
//! call's deadline. A mod that loops forever is **stopped inside the engine**
//! at the wall-clock budget and reported as a hook error; the worker then
//! rebuilds the runtime and reloads the mods so the next call starts from a
//! clean heap. The calling thread's own deadline is only a backstop for work
//! QuickJS cannot interrupt, such as a downstream executor blocked in a native
//! call; that path still detaches the worker. See [`ModLimits`] for the exact
//! ceilings and [`MODS_MEMORY_LIMIT_ENV`] for the heap override.
//!
//! After [`DEFAULT_TIMEOUT_BREAKER_LIMIT`] wall-clock timeouts (overridable
//! with `CODEREX_MODS_TIMEOUT_LIMIT`), the runtime's circuit breaker disables
//! mods for the rest of the session: no worker is started again and a gating
//! `fire` fails closed with an explanatory error. Engine interruption and the
//! caller backstop both count; ordinary hook errors and heap/stack errors do
//! not. See [`ModRuntime::is_disabled`].
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
//! # Async hooks
//!
//! A hook may be `async` and `await` `next(e)`; the runtime drives the
//! returned promise to completion under the same wall-clock budget.

mod executor;
mod runtime;
mod transform;

pub use executor::{JsHookExecutor, ModUiHandle};
pub use runtime::{
    ALLOWED_ENV_VARS, DEFAULT_MEMORY_LIMIT, DEFAULT_TIMEOUT_BREAKER_LIMIT,
    MAX_PENDING_PROMPTS_PER_TURN, MAX_SUBMISSIONS_PER_HOOK_CALL, MIN_MEMORY_LIMIT, MODS_DIR_ENV,
    MODS_ENV, MODS_MEMORY_LIMIT_ENV, MODS_TIMEOUT_LIMIT_ENV, ModInfo, ModLimits, ModRuntime,
    ModRuntimeBuilder, ModSession, ModSubmitQueue, is_enabled, mods_dir,
};
