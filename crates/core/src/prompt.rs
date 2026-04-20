//! Prompt assembly from static/dynamic blocks with token estimation.
//!
//! Base prompt logic lives in `prompts::base`. This module re-exports the
//! public API so existing callers can continue using `crate::prompt::*`.

pub use crate::prompts::base::*;
