//! Live E2E: basic chat without tool calls.

mod live;

use coderex_core::engine::TerminalReason;
use coderex_core::types::EngineEvent;

#[tokio::test]
#[ignore = "live LLM test — run with `source start.kimi.sh && cargo test -p coderex-cli -- --ignored`"]
async fn basic_chat_responds() {
    if !live::have_live_env() {
        eprintln!("{}", live::skip_msg());
        return;
    }

    let mut rig = live::live_rig().await;
    let reason = rig
        .submit("Say exactly the word 'pong' and nothing else.")
        .await;
    assert!(reason.is_ok(), "submit failed: {reason:?}");
    assert_eq!(
        reason.unwrap(),
        TerminalReason::Completed,
        "expected Completed, got different terminal reason"
    );

    let events = rig.collect_events().await;
    let text = live::collect_text(&events);

    assert!(
        text.to_lowercase().contains("pong"),
        "expected response to contain 'pong', got: {text}"
    );

    assert!(
        events.iter().any(|e| matches!(e, EngineEvent::SessionEnd)),
        "expected SessionEnd event"
    );

    eprintln!("Test used model: {:?}", live::live_model());
}

#[tokio::test]
#[ignore = "live LLM test — run with `source start.kimi.sh && cargo test -p coderex-cli -- --ignored`"]
async fn multi_turn_remembers_context() {
    if !live::have_live_env() {
        eprintln!("{}", live::skip_msg());
        return;
    }

    let mut rig = live::live_rig().await;

    // Turn 1: ask to remember
    let r1 = rig
        .submit("Remember the secret word is 'apple'. Just say 'ok'.")
        .await;
    assert!(r1.is_ok(), "turn 1 failed: {r1:?}");
    let events1 = rig.collect_events().await;
    let text1 = live::collect_text(&events1);
    assert!(
        text1.to_lowercase().contains("ok"),
        "turn 1 should say ok, got: {text1}"
    );

    // Turn 2: ask what the word was
    let r2 = rig
        .submit("What was the secret word I asked you to remember? Reply with just the word.")
        .await;
    assert!(r2.is_ok(), "turn 2 failed: {r2:?}");
    let events2 = rig.collect_events().await;
    let text2 = live::collect_text(&events2);
    assert!(
        text2.to_lowercase().contains("apple"),
        "expected 'apple' in turn 2 response, got: {text2}"
    );
}
