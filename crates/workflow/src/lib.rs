//! CodeRex workflows — deterministic JavaScript that orchestrates sub-agents.
//!
//! A workflow is a script the model writes: metadata in a literal `meta`
//! export, then a body that calls `agent()` to spawn sub-agents and `parallel`,
//! `pipeline`, `phase`, `log`, and `budget` to arrange them. Behind it is a
//! runtime that has to be trustworthy in three specific ways:
//!
//! - **Deterministic.** `Date.now()` and `Math.random()` are unavailable,
//!   because a run that resumes replays what it already did and a script that
//!   reads the clock or a random number cannot be replayed. The same script,
//!   arguments, and call sequence produce the same keys in the same order.
//! - **Resumable.** Every `agent()` call gets a key chained onto the call
//!   before it, and results are journaled as they land. A resumed run replays
//!   the unchanged prefix and re-runs from the first thing that changed.
//! - **Bounded.** Concurrency is gated, total agent calls are capped, and a
//!   token budget is a hard ceiling rather than advice, so a loop that never
//!   terminates stops with an explanation instead of a bill.

#![warn(missing_docs)]

pub mod journal;
pub mod meta;

pub use journal::{CallOptions, Entry, Journal, RestoreState, key_for};
pub use meta::{Meta, MetaError, Phase, read};
