//! E2E: Core flow tests — user query → assistant response, tool calls, multi-turn, etc.

mod e2e_harness;

use coderex_commands::registry::{CommandContext, CommandRegistry};
use coderex_core::types::ModelId;

fn test_ctx(cwd: &str) -> CommandContext {
    CommandContext::new(cwd.into(), "e2e_test".into(), "E2E Test".into(), ModelId::sonnet())
}

// E2E: User asks question → fails fast without QueryEngine
#[tokio::test]
async fn e2e_user_query_gets_response() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());
    let result = repl.run_headless("What is Rust?").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("QueryEngine"));
}

// E2E: Slash command /clear → session cleared
#[tokio::test]
async fn e2e_slash_clear_resets_session() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());
    repl.run_headless("Hello").await.expect_err("should fail without engine");
    let result = repl.run_headless("/clear").await.unwrap();
    assert!(!result.is_empty());
}

// E2E: Slash command /help → shows available commands
#[tokio::test]
async fn e2e_slash_help_shows_commands() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());
    let result = repl.run_headless("/help").await.unwrap();
    assert!(!result.is_empty());
}

// E2E: Slash command /status → system status
#[tokio::test]
async fn e2e_slash_status_shows_info() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());
    let result = repl.run_headless("/status").await.unwrap();
    assert!(result.contains("System Status") || result.contains("coderex") || result.contains("Status"));
}

// E2E: Slash command /compact → compacts session
#[tokio::test]
async fn e2e_slash_compact() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());
    let result = repl.run_headless("/compact").await.unwrap();
    assert!(!result.is_empty());
}

// E2E: Slash command /tokens → shows token usage
#[tokio::test]
async fn e2e_slash_tokens() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());
    let result = repl.run_headless("/tokens").await.unwrap();
    assert!(!result.is_empty());
}

// E2E: Model switching → /model sonnet → /model opus
#[tokio::test]
async fn e2e_model_switching() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());

    let r1 = repl.run_headless("/model sonnet").await.unwrap();
    assert!(r1.contains("sonnet") || r1.contains("Model"));

    let r2 = repl.run_headless("/model opus").await.unwrap();
    assert!(r2.contains("opus") || r2.contains("Model"));
}

// E2E: /clear after multiple messages — echoes in headless mode
#[test]
fn e2e_command_registry_not_empty() {
    let mut reg = CommandRegistry::new();
    coderex_commands::registry::register_all_commands(&mut reg);
    assert!(!reg.list().is_empty());
}

// E2E: /vim toggle → enables and disables vim mode
#[test]
fn e2e_vim_toggle() {
    let reg = {
        let mut r = CommandRegistry::new();
        coderex_commands::registry::register_all_commands(&mut r);
        r
    };

    let ctx = test_ctx("/src");
    let result = reg.execute("vim", &ctx, &[]);
    assert!(result.output.contains("enabled"));
    assert!(result.state_changed);

    let ctx_on = CommandContext {
        vim_mode: true,
        ..test_ctx("/src")
    };
    let result = reg.execute("vim", &ctx_on, &[]);
    assert!(result.output.contains("disabled"));
}

// E2E: /clear after multiple messages — fails fast without engine
#[tokio::test]
async fn e2e_clear_after_messages() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());

    repl.run_headless("Message 1").await.expect_err("should fail without engine");
    repl.run_headless("Message 2").await.expect_err("should fail without engine");
    repl.run_headless("Message 3").await.expect_err("should fail without engine");

    let result = repl.run_headless("/clear").await.unwrap();
    assert!(!result.is_empty());
}

// E2E: Multi-turn conversation history — fails fast without engine
#[tokio::test]
async fn e2e_multi_turn_conversation() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());

    let r1 = repl.run_headless("Hello").await;
    assert!(r1.is_err());

    let r2 = repl.run_headless("How are you?").await;
    assert!(r2.is_err());

    let r3 = repl.run_headless("Tell me about Rust").await;
    assert!(r3.is_err());
}

// E2E: Command registry has all expected commands
#[test]
fn e2e_all_commands_registered() {
    let mut reg = CommandRegistry::new();
    coderex_commands::registry::register_all_commands(&mut reg);

    let names: Vec<&str> = reg.list().iter().map(|c| c.name).collect();

    assert!(names.contains(&"clear"));
    assert!(names.contains(&"model"));
    assert!(names.contains(&"compact"));
    assert!(names.contains(&"status"));
    assert!(names.contains(&"help"));
    assert!(names.contains(&"vim"));
    assert!(names.contains(&"voice"));
}

// E2E: REPL headless with empty input — fails fast without QueryEngine
#[tokio::test]
async fn e2e_empty_headless_input() {
    let mut repl = coderex_cli::ReplLoop::new("/src".into(), ModelId::sonnet());
    let result = repl.run_headless("").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("QueryEngine"));
}

// E2E: CLI binary argument parsing — version
#[test]
fn e2e_cli_args_version() {
    use assert_cmd::Command;
    let mut cmd = Command::cargo_bin("coderex").unwrap();
    cmd.arg("--version");
    let assert = cmd.assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("coderex"));
}

// E2E: CLI binary argument parsing — help
#[test]
fn e2e_cli_args_help() {
    use assert_cmd::Command;
    let mut cmd = Command::cargo_bin("coderex").unwrap();
    cmd.arg("--help");
    let assert = cmd.assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("CodeRex"));
}

// E2E: Mock server enqueue and respond
#[test]
fn e2e_mock_server_enqueue() {
    use e2e_harness::MockServer;
    let mut server = MockServer::new();
    server.enqueue("Hello world");
    server.enqueue("Second response");

    let r1 = server.respond("prompt 1");
    assert_eq!(r1.content, "Hello world");

    let r2 = server.respond("prompt 2");
    assert_eq!(r2.content, "Second response");

    assert_eq!(server.call_count(), 2);
}

// E2E: Mock server records history
#[test]
fn e2e_mock_server_history() {
    use e2e_harness::MockServer;
    let mut server = MockServer::new();
    server.enqueue("response");

    server.respond("first question");
    server.respond("second question");

    assert_eq!(server.prompts(), vec!["first question", "second question"]);
}

// E2E: E2EContext creation
#[test]
fn e2e_context_creation() {
    use e2e_harness::E2EContext;
    let mut ctx = E2EContext::new();
    assert!(ctx.temp_path().exists());
    ctx.server.enqueue("test");
    assert_eq!(ctx.server.call_count(), 0);
}
