//! Test utilities for CodeRex.
//!
//! Provides [`TestRig`] for spinning up a `QueryEngine` with mock
//! dependencies, snapshot assertion helpers, and a wire-protocol
//! client for E2E-style tests.

#![warn(missing_docs)]

pub mod rig;
pub mod snapshot;
pub mod wire_client;
pub mod mock_api;
pub mod eval;

pub use rig::{TestRig, TestRigBuilder, StubOkToolExecutor};
pub use wire_client::{WireTestClient, WireTestClientBuilder};
pub use mock_api::{MockApiClient, MockResponseType, mock_response, mock_response_with_tool_calls, mock_streaming_response};
pub use eval::{EvalScenario, EvalAssertion, EvalResult, AssertionResult, EventType, run_eval};
