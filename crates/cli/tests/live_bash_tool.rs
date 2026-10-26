//! Live E2E: bash tool invocation.

mod live;

use coderex_core::engine::TerminalReason;

#[tokio::test]
async fn bash_tool_lists_files() {
    if !live::have_live_env() {
        eprintln!("{}", live::skip_msg());
        return;
    }

    let mut rig = live::live_rig().await;
    let reason = rig
        .submit("List the files in the current directory using the bash tool and tell me how many files/directories you see. Just give a number.")
        .await;
    assert!(reason.is_ok(), "submit failed: {reason:?}");
    assert_eq!(
        reason.unwrap(),
        TerminalReason::Completed,
        "expected Completed"
    );

    let events = rig.drain_events();

    assert!(
        live::has_tool_call(&events, "bash"),
        "expected a bash tool call"
    );

    let text = live::collect_text(&events);
    // The response should contain some digits (file count)
    assert!(
        text.chars().any(|c| c.is_ascii_digit()),
        "expected response to contain a number, got: {text}"
    );

    eprintln!("Response: {text}");
}

#[tokio::test]
async fn bash_tool_echo_test() {
    if !live::have_live_env() {
        eprintln!("{}", live::skip_msg());
        return;
    }

    let mut rig = live::live_rig().await;
    let reason = rig
        .submit("Use the bash tool to echo 'hello-from-live-test'. Then report back exactly what was echoed.")
        .await;
    assert!(reason.is_ok(), "submit failed: {reason:?}");

    let events = rig.drain_events();

    assert!(
        live::has_tool_call(&events, "bash"),
        "expected a bash tool call"
    );

    let text = live::collect_text(&events);
    assert!(
        text.contains("hello-from-live-test"),
        "expected echoed text in response, got: {text}"
    );
}
