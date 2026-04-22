//! AI Eval framework — regression-test agent behavior against mock responses.
//!
//! Provides [`EvalScenario`] and [`EvalAssertion`] for defining eval tests
//! that assert on tool calls, file outputs, and event sequences produced
//! by the QueryEngine when fed pre-determined LLM responses.
//!
//! # Example
//!
//! ```rust,ignore
//! use coderex_test_utils::{EvalScenario, EvalAssertion, eval_test};
//!
//! # async fn example() {
//! eval_test!("read_file_scenario",
//!     responses: vec![
//!         r#"[{"id":"tc_1","name":"Read","input":{"path":"test.txt"}}]"#.into()
//!     ],
//!     assertions: vec![
//!         EvalAssertion::ToolCalled { name: "Read".into() },
//!         EvalAssertion::OutputContains { text: "stub ok: Read".into() },
//!     ]
//! ).await;
//! # }
//! ```

use crate::mock_api::{MockApiClient, MockResponseType, mock_response, mock_response_with_tool_calls};
use crate::rig::{StubOkToolExecutor, TestRigBuilder};
use coderex_core::types::{EngineEvent, ToolCall};

/// A single eval scenario.
pub struct EvalScenario {
    /// Human-readable name.
    pub name: String,
    /// Mock API responses (in order).
    pub responses: Vec<String>,
    /// Assertions to verify.
    pub assertions: Vec<EvalAssertion>,
    /// Optional tool executor override.
    pub tool_executor: Option<Box<dyn coderex_core::tool_executor::ToolExecutor + Send + Sync>>,
}

impl std::fmt::Debug for EvalScenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalScenario")
            .field("name", &self.name)
            .field("responses", &self.responses)
            .field("assertions", &self.assertions)
            .finish_non_exhaustive()
    }
}

/// An assertion about engine behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum EvalAssertion {
    /// Assert a specific tool was called.
    ToolCalled { name: String },
    /// Assert a tool was NOT called.
    ToolNotCalled { name: String },
    /// Assert the output contains text.
    OutputContains { text: String },
    /// Assert the output does NOT contain text.
    OutputNotContains { text: String },
    /// Assert at least N text deltas were emitted.
    TextDeltaCount { min: usize },
    /// Assert events contain a pattern in order.
    EventSequence { types: Vec<EventType> },
    /// Assert the turn terminated with a specific reason.
    TerminalReason { reason: String },
}

/// Simplified event type for sequence assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventType {
    /// Text delta.
    TextDelta,
    /// Tool call start.
    ToolCallStart,
    /// Tool call end.
    ToolCallEnd,
    /// Session end.
    SessionEnd,
    /// Compact boundary.
    CompactBoundary,
}

/// Result of running an eval scenario.
#[derive(Debug, Clone)]
pub struct EvalResult {
    /// Scenario name.
    pub name: String,
    /// Whether all assertions passed.
    pub passed: bool,
    /// Individual assertion results.
    pub assertion_results: Vec<AssertionResult>,
    /// Events captured during the run.
    pub events: Vec<EngineEvent>,
    /// Terminal reason (if any).
    pub terminal_reason: Option<String>,
}

/// Result of a single assertion.
#[derive(Debug, Clone, PartialEq)]
pub struct AssertionResult {
    /// The assertion that was checked.
    pub assertion: EvalAssertion,
    /// Whether it passed.
    pub passed: bool,
    /// Human-readable explanation.
    pub message: String,
}

/// Run a single eval scenario and return the result.
pub async fn run_eval(scenario: EvalScenario) -> EvalResult {
    let api_responses: Vec<MockResponseType> = scenario
        .responses
        .into_iter()
        .map(|r| {
            // Try to parse as a JSON array of ToolCall first
            if let Ok(tool_calls) = serde_json::from_str::<Vec<ToolCall>>(&r) {
                MockResponseType::Static(mock_response_with_tool_calls("", tool_calls))
            } else {
                MockResponseType::Static(mock_response(r))
            }
        })
        .collect();

    let api = MockApiClient::with_mixed(api_responses);

    let mut builder = TestRigBuilder::new().with_api_client(Box::new(api));

    if let Some(tools) = scenario.tool_executor {
        builder = builder.with_tool_executor(tools);
    } else {
        builder = builder.with_tool_executor(Box::new(StubOkToolExecutor));
    }

    let mut rig = builder.build().await;

    let reason = rig.submit("eval prompt").await;
    let events = rig.drain_events();

    let terminal_reason = reason.as_ref().ok().map(|r| r.to_string());

    let assertion_results: Vec<AssertionResult> = scenario
        .assertions
        .iter()
        .map(|a| check_assertion(a, &events, &reason, &terminal_reason))
        .collect();

    let passed = assertion_results.iter().all(|r| r.passed);

    EvalResult {
        name: scenario.name,
        passed,
        assertion_results,
        events,
        terminal_reason,
    }
}

fn check_assertion(
    assertion: &EvalAssertion,
    events: &[EngineEvent],
    reason: &Result<coderex_core::engine::TerminalReason, String>,
    _terminal_reason: &Option<String>,
) -> AssertionResult {
    match assertion {
        EvalAssertion::ToolCalled { name } => {
            let found = events.iter().any(|e| matches!(
                e,
                EngineEvent::ToolCallStart { name: n, .. } if n == name
            ));
            AssertionResult {
                assertion: assertion.clone(),
                passed: found,
                message: if found {
                    format!("tool '{name}' was called")
                } else {
                    format!("tool '{name}' was NOT called")
                },
            }
        }
        EvalAssertion::ToolNotCalled { name } => {
            let found = events.iter().any(|e| matches!(
                e,
                EngineEvent::ToolCallStart { name: n, .. } if n == name
            ));
            AssertionResult {
                assertion: assertion.clone(),
                passed: !found,
                message: if !found {
                    format!("tool '{name}' was not called (as expected)")
                } else {
                    format!("tool '{name}' was called unexpectedly")
                },
            }
        }
        EvalAssertion::OutputContains { text } => {
            let found = events.iter().any(|e| match e {
                EngineEvent::TextDelta(delta) => delta.contains(text),
                EngineEvent::ToolCallEnd { result, .. } => result.output.contains(text),
                _ => false,
            });
            AssertionResult {
                assertion: assertion.clone(),
                passed: found,
                message: if found {
                    format!("output contains '{text}'")
                } else {
                    format!("output does NOT contain '{text}'")
                },
            }
        }
        EvalAssertion::OutputNotContains { text } => {
            let found = events.iter().any(|e| match e {
                EngineEvent::TextDelta(delta) => delta.contains(text),
                EngineEvent::ToolCallEnd { result, .. } => result.output.contains(text),
                _ => false,
            });
            AssertionResult {
                assertion: assertion.clone(),
                passed: !found,
                message: if !found {
                    format!("output does not contain '{text}' (as expected)")
                } else {
                    format!("output unexpectedly contains '{text}'")
                },
            }
        }
        EvalAssertion::TextDeltaCount { min } => {
            let count = events
                .iter()
                .filter(|e| matches!(e, EngineEvent::TextDelta { .. }))
                .count();
            AssertionResult {
                assertion: assertion.clone(),
                passed: count >= *min,
                message: format!("text delta count: {count} (expected >= {min})"),
            }
        }
        EvalAssertion::EventSequence { types } => {
            let event_types: Vec<EventType> = events
                .iter()
                .filter_map(|e| match e {
                    EngineEvent::TextDelta { .. } => Some(EventType::TextDelta),
                    EngineEvent::ToolCallStart { .. } => Some(EventType::ToolCallStart),
                    EngineEvent::ToolCallEnd { .. } => Some(EventType::ToolCallEnd),
                    EngineEvent::SessionEnd => Some(EventType::SessionEnd),
                    EngineEvent::CompactBoundary => Some(EventType::CompactBoundary),
                    _ => None,
                })
                .collect();

            let passed = types.is_empty() || event_types.windows(types.len()).any(|w| w == types.as_slice());
            AssertionResult {
                assertion: assertion.clone(),
                passed,
                message: if passed {
                    format!("event sequence {types:?} found")
                } else {
                    format!("event sequence {types:?} NOT found in {event_types:?}")
                },
            }
        }
        EvalAssertion::TerminalReason { reason: expected } => {
            let actual = reason.as_ref().map(|r| r.to_string()).unwrap_or_else(|e| format!("error: {e}"));
            let passed = actual == *expected;
            AssertionResult {
                assertion: assertion.clone(),
                passed,
                message: format!("terminal reason: {actual} (expected: {expected})"),
            }
        }
    }
}

/// Define and run an eval test inline.
///
/// # Example
///
/// ```rust,ignore
/// use coderex_test_utils::{eval_test, EvalAssertion};
///
/// # async fn example() {
/// eval_test!("my_eval",
///     responses: vec!["hello".into()],
///     assertions: vec![
///         EvalAssertion::TextDeltaCount { min: 1 },
///     ]
/// ).await;
/// # }
/// ```
#[macro_export]
macro_rules! eval_test {
    ($name:expr, responses: $responses:expr, assertions: $assertions:expr $(,)?) => {{
        let scenario = $crate::eval::EvalScenario {
            name: $name.into(),
            responses: $responses,
            assertions: $assertions,
            tool_executor: None,
        };
        $crate::eval::run_eval(scenario).await
    }};
    ($name:expr, responses: $responses:expr, assertions: $assertions:expr, tool_executor: $tools:expr $(,)?) => {{
        let scenario = $crate::eval::EvalScenario {
            name: $name.into(),
            responses: $responses,
            assertions: $assertions,
            tool_executor: Some($tools),
        };
        $crate::eval::run_eval(scenario).await
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn eval_tool_called_passes() {
        let result = eval_test!(
            "tool_call_eval",
            responses: vec![
                r#"[{"id":"tc_1","name":"Read","input":{"path":"test.txt"}}]"#.into(),
                "Done.".into(),
            ],
            assertions: vec![
                EvalAssertion::ToolCalled { name: "Read".into() },
            ]
        );

        assert!(result.passed, "eval should pass: {:?}", result.assertion_results);
    }

    #[tokio::test]
    async fn eval_tool_not_called_passes() {
        let result = eval_test!(
            "no_tool_eval",
            responses: vec!["Hello, world!".into()],
            assertions: vec![
                EvalAssertion::ToolNotCalled { name: "Read".into() },
            ]
        );

        assert!(result.passed, "eval should pass: {:?}", result.assertion_results);
    }

    #[tokio::test]
    async fn eval_output_contains_passes() {
        let result = eval_test!(
            "output_contains_eval",
            responses: vec![
                r#"[{"id":"tc_1","name":"Read","input":{"path":"test.txt"}}]"#.into(),
                "Done.".into(),
            ],
            assertions: vec![
                EvalAssertion::OutputContains { text: "stub ok: Read".into() },
            ]
        );

        assert!(result.passed, "eval should pass: {:?}", result.assertion_results);
    }
}
