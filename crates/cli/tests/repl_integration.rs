//! REPL / UI integration tests — covering ReplLoop construction, headless execution,
//! slash command dispatch, command parsing, and CLI config application.
//!
//! These tests correspond to the equivalence test plan Module 14:
//! 14.1 ReplLoop construction, 14.2 Headless execution, 14.3 Slash commands,
//! 14.4 Command parsing, 14.5 CLI config effects, 14.6 Session state

use coderex_cli::repl::{ReplLoop, ReplSession, parse_input};
use coderex_core::permissions::PermissionMode;
use coderex_core::types::ModelId;
use coderex_test_utils::{MockApiClient, mock_response};

// ═══════════════════════════════════════════════════════════════════════════
//  14.1 ReplLoop Construction
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn repl_loop_new_creates_with_commands_registered() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    // Verify commands are registered by running one via run_headless
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/clear")).unwrap();
    assert!(
        result.contains("cleared"),
        "expected /clear to work, got: {result}"
    );
}

#[tokio::test]
async fn repl_loop_with_engine_creates_valid_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockApiClient::new(vec![mock_response("hello from mock")]);
    let mut repl = ReplLoop::with_engine(
        Box::new(mock),
        tmp.path().to_str().unwrap().to_string(),
        ModelId::sonnet(),
    );

    // Verify slash commands still work
    let result = repl.run_headless("/session").await.unwrap();
    assert!(
        result.contains("Session:"),
        "expected /session to work with engine, got: {result}"
    );
}

/// A headless turn now leaves a transcript behind, so `--print` sessions can
/// be resumed and found by `/resume`.
#[tokio::test]
async fn headless_turn_persists_a_resumable_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_str().unwrap().to_string();
    let mock = MockApiClient::new(vec![mock_response("persisted answer")]);
    let mut repl = ReplLoop::with_engine(Box::new(mock), cwd.clone(), ModelId::sonnet());

    repl.run_headless("hello").await.unwrap();

    let dir = coderex_cli::session_resume::transcript_dir(&cwd);
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("the transcript directory should exist")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(entries.len(), 1, "exactly one transcript should be written");
    let content = std::fs::read_to_string(entries[0].path()).unwrap();
    assert!(
        content.contains("persisted answer"),
        "the transcript should carry the turn: {content}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.2 Headless Execution — Slash Commands
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn run_headless_clear_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/clear")).unwrap();
    assert!(
        result.contains("cleared"),
        "expected 'cleared', got: {result}"
    );
}

#[test]
fn run_headless_exit_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/exit")).unwrap();
    assert!(
        result.contains("Exiting"),
        "expected 'Exiting', got: {result}"
    );
}

#[test]
fn run_headless_session_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/session")).unwrap();
    assert!(
        result.contains("Session:"),
        "expected session info, got: {result}"
    );
    assert!(
        result.contains("claude-sonnet-4-6"),
        "expected model name in output"
    );
}

#[test]
fn run_headless_advisor_mode_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/advisor")).unwrap();
    assert!(
        result.contains("Advisor mode"),
        "expected advisor mode, got: {result}"
    );
}

#[test]
fn run_headless_btw_command_with_args() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt
        .block_on(repl.run_headless("/btw this is important"))
        .unwrap();
    assert!(
        result.contains("Added context"),
        "expected context addition, got: {result}"
    );
}

#[test]
fn run_headless_btw_without_args_errors() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/btw")).unwrap();
    // `/btw` now declares its arity, so the registry rejects the call before
    // the handler's own usage message can run.
    assert!(
        result.contains("requires at least 1 argument"),
        "expected the arity error, got: {result}"
    );
}

#[test]
fn run_headless_unknown_command_returns_error() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/nonexistent")).unwrap();
    assert!(
        result.contains("Unknown command"),
        "expected unknown command error, got: {result}"
    );
}

#[test]
fn run_headless_good_claude_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/good-claude")).unwrap();
    assert!(
        result.contains("Thanks"),
        "expected thanks message, got: {result}"
    );
}

#[test]
fn run_headless_env_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/env")).unwrap();
    assert!(
        result.contains("Environment"),
        "expected env output, got: {result}"
    );
}

#[test]
fn run_headless_diff_command_outside_a_repository_says_so() {
    // A directory that is not in a repository: the honest answer is that there
    // is nothing to diff, not that nothing changed.
    let dir = tempfile::tempdir().unwrap();
    let mut repl = ReplLoop::new(dir.path().to_string_lossy().into_owned(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/diff")).unwrap();
    assert!(
        result.contains("Not a git repository"),
        "expected the reason, got: {result}"
    );
}

#[test]
fn run_headless_diff_command_in_a_clean_repository_shows_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["commit", "--quiet", "--allow-empty", "-m", "init"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git");
        assert!(status.status.success(), "{args:?} failed");
    }

    let mut repl = ReplLoop::new(dir.path().to_string_lossy().into_owned(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/diff")).unwrap();
    assert!(
        result.contains("No changes"),
        "expected no changes, got: {result}"
    );
}

#[test]
fn run_headless_issue_create_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt
        .block_on(repl.run_headless("/issue create fix bug"))
        .unwrap();
    assert!(
        result.contains("fix bug"),
        "expected issue creation, got: {result}"
    );
}

#[test]
fn run_headless_issue_list_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/issue list")).unwrap();
    assert!(
        result.contains("No open issues"),
        "expected empty issues list, got: {result}"
    );
}

#[test]
fn run_headless_bughunter_mode_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/bughunter")).unwrap();
    assert!(
        result.contains("Bug Hunter"),
        "expected bug hunter mode, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.3 Headless Execution — Pipe Operator
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn run_headless_pipe_operator() {
    let dir = tempfile::tempdir().unwrap();
    let mut repl = ReplLoop::new(dir.path().to_string_lossy().into_owned(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Two commands that work in any directory — piping is what is under test
    // here, and a command that depends on the checkout being a repository
    // would make the test depend on where it runs.
    let result = rt.block_on(repl.run_headless("/version | /env")).unwrap();

    assert!(
        result.contains("---"),
        "expected pipe separator, got: {result}"
    );
    assert!(
        result.contains("CodeRex") && result.contains("Environment"),
        "expected both commands' output, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.4 Headless Execution — Non-Slash Input
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn run_headless_non_slash_without_engine_errors() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("hello world"));
    assert!(
        result.is_err(),
        "expected error for non-slash input without engine"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("No QueryEngine configured"),
        "expected 'No QueryEngine configured', got: {err}"
    );
}

#[tokio::test]
async fn run_headless_non_slash_with_engine_routes_to_query() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockApiClient::new(vec![mock_response("AI response")]);
    let mut repl = ReplLoop::with_engine(
        Box::new(mock),
        tmp.path().to_str().unwrap().to_string(),
        ModelId::sonnet(),
    );
    let result = repl.run_headless("hello world").await.unwrap();
    assert!(
        result.contains("AI response") || result.is_empty(),
        "expected query engine response, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.5 CLI Config Application
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn apply_cli_config_no_slash_commands_disables_all() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    repl.with_no_slash_commands(true);
    repl.apply_cli_config();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/clear")).unwrap();
    assert!(
        result.contains("Unknown command"),
        "expected all slash commands disabled, got: {result}"
    );
}

#[tokio::test]
async fn apply_cli_config_skip_permissions_does_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockApiClient::new(vec![mock_response("test")]);
    let mut repl = ReplLoop::with_engine(
        Box::new(mock),
        tmp.path().to_str().unwrap().to_string(),
        ModelId::sonnet(),
    );
    repl.with_skip_permissions(true);
    repl.apply_cli_config();
    // Should not panic; we verify the REPL is still functional
    let result = repl.run_headless("/session").await.unwrap();
    assert!(result.contains("Session:"));
}

#[tokio::test]
async fn apply_cli_config_permission_mode_does_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockApiClient::new(vec![mock_response("test")]);
    let mut repl = ReplLoop::with_engine(
        Box::new(mock),
        tmp.path().to_str().unwrap().to_string(),
        ModelId::sonnet(),
    );
    repl.with_permission_mode(Some(PermissionMode::BypassPermissions));
    repl.apply_cli_config();
    let result = repl.run_headless("/session").await.unwrap();
    assert!(result.contains("Session:"));
}

#[tokio::test]
async fn apply_cli_config_no_persistence_does_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = MockApiClient::new(vec![mock_response("test")]);
    let mut repl = ReplLoop::with_engine(
        Box::new(mock),
        tmp.path().to_str().unwrap().to_string(),
        ModelId::sonnet(),
    );
    repl.with_no_persistence(true);
    repl.apply_cli_config();
    let result = repl.run_headless("/session").await.unwrap();
    assert!(result.contains("Session:"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.6 Command Parsing
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn parse_input_empty_returns_none() {
    assert!(parse_input("").is_none());
    assert!(parse_input("   ").is_none());
}

#[test]
fn parse_input_slash_command_no_args() {
    let result = parse_input("/clear").unwrap();
    assert_eq!(result.0, "clear");
    assert_eq!(result.1, [] as [std::string::String; 0]);
}

#[test]
fn parse_input_slash_command_with_args() {
    let result = parse_input("/model sonnet").unwrap();
    assert_eq!(result.0, "model");
    assert_eq!(result.1, vec!["sonnet"]);
}

#[test]
fn parse_input_slash_command_multiple_args() {
    let result = parse_input("/btw this is important").unwrap();
    assert_eq!(result.0, "btw");
    assert_eq!(result.1, vec!["this", "is", "important"]);
}

#[test]
fn parse_input_regular_message() {
    let result = parse_input("Hello, world!").unwrap();
    assert_eq!(result.0, "message");
    assert_eq!(result.1, vec!["Hello, world!"]);
}

#[test]
fn parse_input_message_with_leading_whitespace() {
    let result = parse_input("   hello there   ").unwrap();
    assert_eq!(result.0, "message");
    assert_eq!(result.1, vec!["hello there"]);
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.7 ReplSession
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn repl_session_creation() {
    let session = ReplSession::new("sess_123".into(), "Test Session".into());
    assert_eq!(session.id, "sess_123");
    assert_eq!(session.title, "Test Session");
    assert!(session.active);
    assert_eq!(session.message_count, 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.8 Command History via run_headless
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn run_headless_records_command_history() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Run a few commands
    rt.block_on(repl.run_headless("/clear")).unwrap();
    rt.block_on(repl.run_headless("/session")).unwrap();
    rt.block_on(repl.run_headless("/env")).unwrap();

    // Run history command to verify it was recorded
    let result = rt.block_on(repl.run_headless("/history 3")).unwrap();
    assert!(
        result.contains('3') || result.contains("commands"),
        "expected history output, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.9 Feedback Command
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn run_headless_feedback_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/feedback")).unwrap();
    assert!(
        result.contains("Use /feedback"),
        "expected feedback help, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.10 OAuth Refresh Command
// ═══════════════════════════════════════════════════════════════════════════

/// The command reports the credential state and says plainly that no token
/// was refreshed. It used to claim a refresh it never performed; this asserts
/// the honest answer, so the claim cannot come back without the flow.
#[test]
fn run_headless_oauth_refresh_reports_credentials_without_claiming_a_refresh() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/oauth-refresh")).unwrap();

    assert!(
        result.contains("No OAuth token was refreshed") || result.contains("No credential found"),
        "the command must not claim a refresh it did not do, got: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  14.11 Issue View Command
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn run_headless_issue_view_command() {
    let mut repl = ReplLoop::new("/tmp".into(), ModelId::sonnet());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(repl.run_headless("/issue 42")).unwrap();
    assert!(
        result.contains("42"),
        "expected issue number in output, got: {result}"
    );
}
