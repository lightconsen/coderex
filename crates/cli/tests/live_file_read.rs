//! Live E2E: file read tool invocation.

mod live;

use coderex_core::engine::TerminalReason;
use std::io::Write;

#[tokio::test]
#[ignore = "live LLM test — run with `source start.kimi.sh && cargo test -p coderex-cli -- --ignored`"]
async fn read_tool_reads_file() {
    if !live::have_live_env() {
        eprintln!("{}", live::skip_msg());
        return;
    }

    // Write a known file into the temp directory
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let test_file = temp_dir.path().join("test_live_read.txt");
    {
        let mut f = std::fs::File::create(&test_file).expect("create file");
        f.write_all(b"the magic phrase is xyzzy").expect("write");
    }

    let mut rig = live::live_rig().await;
    let reason = rig
        .submit(&format!(
            "Read the file '{}' and tell me the magic phrase inside it.",
            test_file.display()
        ))
        .await;
    assert!(reason.is_ok(), "submit failed: {:?}", reason);
    assert_eq!(reason.unwrap(), TerminalReason::Completed, "expected Completed");

    let events = rig.collect_events().await;

    assert!(
        live::has_tool_call(&events, "read"),
        "expected a read tool call"
    );

    let text = live::collect_text(&events);
    assert!(
        text.to_lowercase().contains("xyzzy"),
        "expected magic phrase in response, got: {}",
        text
    );

    eprintln!("Response: {}", text);
}

#[tokio::test]
#[ignore = "live LLM test — run with `source start.kimi.sh && cargo test -p coderex-cli -- --ignored`"]
async fn read_then_bash_combined() {
    if !live::have_live_env() {
        eprintln!("{}", live::skip_msg());
        return;
    }

    // Create a test file
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let test_file = temp_dir.path().join("combined_test.txt");
    {
        let mut f = std::fs::File::create(&test_file).expect("create file");
        f.write_all(b"42").expect("write");
    }

    let mut rig = live::live_rig().await;
    let reason = rig
        .submit(&format!(
            "Read the file '{}' to get a number, then use bash to echo 'the-answer-is-<number>'. Report back the echo output.",
            test_file.display()
        ))
        .await;
    assert!(reason.is_ok(), "submit failed: {:?}", reason);

    let events = rig.collect_events().await;

    assert!(
        live::has_tool_call(&events, "read"),
        "expected a read tool call"
    );
    assert!(
        live::has_tool_call(&events, "bash"),
        "expected a bash tool call"
    );

    let text = live::collect_text(&events);
    assert!(
        text.contains("the-answer-is-42"),
        "expected combined result in response, got: {}",
        text
    );

    eprintln!("Response: {}", text);
}
