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

pub use runtime::{Runtime, RuntimeHandle, UiEvent};
