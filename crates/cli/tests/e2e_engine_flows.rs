//! E2E engine flow tests.
//!
//! These tests exercise the full QueryEngine with a mock API client,
//! verifying end-to-end behavior without hitting real LLM APIs.

use coderex_core::engine::TerminalReason;
use coderex_core::types::EngineEvent;
use coderex_test_utils::{
    MockApiClient, MockResponseType, StubOkToolExecutor, TestRig, TestRigBuilder, mock_response,
};

#[tokio::test]
async fn engine_completes_simple_prompt() {
    let mut rig = TestRig::new().await;
    let reason = rig.submit("Hello").await.unwrap();

    assert!(matches!(reason, TerminalReason::Completed));

    let events = rig.drain_events();
    assert!(
        events.iter().any(|e| matches!(e, EngineEvent::TextDelta { .. })),
        "expected at least one text delta, got: {events:?}"
    );
}

#[tokio::test]
async fn engine_handles_tool_call() {
    let api = MockApiClient::with_mixed(vec![
        MockResponseType::Static(mock_response(
            r#"[{"id":"tc_1","name":"Read","input":{"path": "hello.txt"}}]"#,
        )),
        MockResponseType::Static(mock_response(
            "Done reading the file.",
        )),
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
        events.iter().any(|e| matches!(e, EngineEvent::ToolCallStart { .. })),
        "expected ToolCallStart"
    );
    assert!(
        events.iter().any(|e| matches!(e, EngineEvent::ToolCallEnd { .. })),
        "expected ToolCallEnd"
    );
}

#[tokio::test]
async fn engine_tracks_api_calls() {
    let api = MockApiClient::with_mixed(vec![
        MockResponseType::Static(mock_response(
            "Hello.",
        )),
    ]);

    let api_clone = api.clone();
    let mut rig = TestRigBuilder::new()
        .with_api_client(Box::new(api))
        .build()
        .await;

    let _ = rig.submit("Say hello").await;

    // The mock API should have been called exactly once
    assert_eq!(api_clone.call_count(), 1);
}

#[tokio::test]
async fn tool_result_contains_stub_output() {
    let api = MockApiClient::with_mixed(vec![
        MockResponseType::Static(mock_response(
            r#"[{"id":"tc_1","name":"Read","input":{"path": "test.txt"}}]"#,
        )),
        MockResponseType::Static(mock_response(
            "All done.",
        )),
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
