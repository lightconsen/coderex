//! E2E engine flow tests.
//!
//! These tests exercise the full QueryEngine with a mock API client,
//! verifying end-to-end behavior without hitting real LLM APIs.

use coderex_core::api_client::StreamEvent;
use coderex_core::engine::{SubmitOptions, TerminalReason};
use coderex_core::types::EngineEvent;
use coderex_core::types::ToolCall;
use coderex_core::types::UsageSummary;
use coderex_test_utils::{
    MockApiClient, MockResponseType, StubOkToolExecutor, TestRig, TestRigBuilder, mock_response,
    mock_response_with_tool_calls,
};

#[tokio::test]
async fn engine_completes_simple_prompt() {
    let mut rig = TestRig::new().await;
    let reason = rig.submit("Hello").await.unwrap();

    assert!(matches!(reason, TerminalReason::Completed));

    let events = rig.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TextDelta { .. })),
        "expected at least one text delta, got: {events:?}"
    );
}

#[tokio::test]
async fn engine_handles_tool_call() {
    let api = MockApiClient::with_mixed(vec![
        MockResponseType::Static(mock_response_with_tool_calls(
            "",
            vec![ToolCall {
                id: "tc_1".into(),
                name: "Read".into(),
                input: serde_json::json!({"path": "hello.txt"}),
            }],
        )),
        MockResponseType::Static(mock_response("Done reading the file.")),
    ]);

    let mut rig = TestRigBuilder::new()
        .with_api_client(Box::new(api))
        .with_tool_executor(Box::new(StubOkToolExecutor))
        .build()
        .await;

    let reason = rig.submit("Read hello.txt").await.unwrap();
    assert!(matches!(reason, TerminalReason::Completed));

    let events = rig.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallStart { .. })),
        "expected ToolCallStart"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallEnd { .. })),
        "expected ToolCallEnd"
    );
}

#[tokio::test]
async fn engine_tracks_api_calls() {
    let api = MockApiClient::with_mixed(vec![MockResponseType::Static(mock_response("Hello."))]);

    let api_clone = api.clone();
    let mut rig = TestRigBuilder::new()
        .with_api_client(Box::new(api))
        .build()
        .await;

    let _ = rig.submit("Say hello").await;

    // The mock API should have been called exactly once
    assert_eq!(api_clone.call_count(), 1);
}

/// A turn that asks for a read-only shell command runs it, hands the result
/// back, and takes the model's answer.
///
/// The mock answers twice because that is what a turn with a tool call is: the
/// model asks, the tool runs, the model answers. Answering with the same tool
/// call every time is not "one turn" — it is a session asking for the same
/// command forever, and the loop detector stops it (see
/// `a_model_that_repeats_one_tool_call_is_stopped`, which asserts exactly
/// that).
#[tokio::test]
async fn a_read_only_shell_command_runs_and_the_turn_takes_the_answer() {
    let api = MockApiClient::with_mixed(vec![
        MockResponseType::Streaming(vec![
            StreamEvent::ToolCallBlockStart {
                id: "tc_1".into(),
                name: "Bash".into(),
            },
            StreamEvent::ToolCallInputDelta {
                id: "tc_1".into(),
                delta: r#"{"command":"ls -la"}"#.into(),
            },
            StreamEvent::ToolCallBlockEnd {
                id: "tc_1".into(),
                input: serde_json::json!({"command": "ls -la"}),
            },
            StreamEvent::UsageSummary(UsageSummary {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }),
            StreamEvent::StreamEnd,
        ]),
        MockResponseType::Streaming(vec![
            StreamEvent::TextDelta("The directory holds the usual files.".into()),
            StreamEvent::UsageSummary(UsageSummary {
                input_tokens: 20,
                output_tokens: 8,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }),
            StreamEvent::StreamEnd,
        ]),
    ]);

    let api_clone = api.clone();
    let mut rig = TestRigBuilder::new()
        .with_api_client(Box::new(api))
        .with_tool_executor(Box::new(StubOkToolExecutor))
        .build()
        .await;

    let reason = rig
        .engine
        .submit_streaming("list files in this dir".into(), SubmitOptions::default())
        .await
        .unwrap();

    assert!(matches!(reason, TerminalReason::Completed), "{reason:?}");
    // One call for the tool call, one for the answer.
    assert_eq!(api_clone.call_count(), 2);

    let events = rig.drain_events();
    let starts = events
        .iter()
        .filter(|event| matches!(event, EngineEvent::ToolCallStart { .. }))
        .count();
    assert_eq!(starts, 1, "the command should run exactly once");

    let ran = events
        .iter()
        .filter(|event| matches!(event, EngineEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ran, 1, "the command should finish exactly once");
}

/// A model that keeps asking for the same command is a session going nowhere,
/// and the engine stops it rather than running the command forever. This is
/// the behaviour the read-only-command test above used to trip over.
#[tokio::test]
async fn a_model_that_repeats_one_tool_call_is_stopped() {
    let repeated_tool_call = || {
        MockResponseType::Streaming(vec![
            StreamEvent::ToolCallBlockStart {
                id: "tc_1".into(),
                name: "Bash".into(),
            },
            StreamEvent::ToolCallBlockEnd {
                id: "tc_1".into(),
                input: serde_json::json!({"command": "ls -la"}),
            },
            StreamEvent::UsageSummary(UsageSummary {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }),
            StreamEvent::StreamEnd,
        ])
    };

    let api = MockApiClient::with_mixed(vec![
        repeated_tool_call(),
        repeated_tool_call(),
        repeated_tool_call(),
        repeated_tool_call(),
        repeated_tool_call(),
    ]);
    let api_clone = api.clone();

    let mut rig = TestRigBuilder::new()
        .with_api_client(Box::new(api))
        .with_tool_executor(Box::new(StubOkToolExecutor))
        .build()
        .await;

    let reason = rig
        .engine
        .submit_streaming("list files in this dir".into(), SubmitOptions::default())
        .await
        .unwrap();

    assert!(
        matches!(reason, TerminalReason::LoopDetected { .. }),
        "expected the loop detector to stop it, got {reason:?}"
    );
    // And it stopped well before the mock ran out of answers.
    assert!(
        api_clone.call_count() < 5,
        "{} calls",
        api_clone.call_count()
    );
}

#[tokio::test]
async fn tool_result_contains_stub_output() {
    let api = MockApiClient::with_mixed(vec![
        MockResponseType::Static(mock_response_with_tool_calls(
            "",
            vec![ToolCall {
                id: "tc_1".into(),
                name: "Read".into(),
                input: serde_json::json!({"path": "test.txt"}),
            }],
        )),
        MockResponseType::Static(mock_response("All done.")),
    ]);

    let mut rig = TestRigBuilder::new()
        .with_api_client(Box::new(api))
        .with_tool_executor(Box::new(StubOkToolExecutor))
        .build()
        .await;

    let _ = rig.submit("Read test.txt").await;

    let events = rig.drain_events();
    let tool_ends: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ToolCallEnd { result, .. } => Some(result.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(tool_ends.len(), 1);
    assert!(tool_ends[0].output.contains("stub ok: Read"));
    assert!(!tool_ends[0].is_error);
}
