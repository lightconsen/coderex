//! CodeRex core types, QueryEngine, state machines, and shared traits.
//!
//! This crate contains the foundational types used throughout the CodeRex
//! system, including message types, tool definitions, session state, and
//! the QueryEngine state machine.

#![warn(missing_docs)]

pub mod types;
pub mod state;
pub mod runtime;
pub mod engine;
pub mod api_client;
pub mod budget;
pub mod cost;
pub mod permissions;
pub mod permission_engine;
pub mod sandbox;

pub use runtime::{Runtime, RuntimeHandle, UiEvent};
pub use engine::{
    QueryEngine, SubmitOptions, TerminalReason, TurnProgress, TurnRuntime, TurnState,
};
