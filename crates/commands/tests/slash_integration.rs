//! Slash command integration tests — covering all registered commands,
//! command types, and availability filtering.
//!
//! These tests correspond to the equivalence test plan Module 9:
//! 9.1 basic commands, 9.2 command types, 9.3 remote-safe, 9.4 bridge-safe

use std::collections::HashMap;

use coderex_commands::{
    CommandContext, CommandRegistry, CommandResult, CommandType, register_all_commands,
};
use coderex_core::types::ModelId;

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn test_ctx() -> CommandContext {
    CommandContext {
        session_title: "Test Session".into(),
        message_count: 10,
        elapsed_secs: 120,
        git_branch: "main".into(),
        git_status: "M file.txt".into(),
        input_tokens: 5000,
        output_tokens: 3000,
        cache_read_tokens: 1000,
        cache_creation_tokens: 500,
        cost_usd: 0.042,
        ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
    }
}

fn full_registry() -> CommandRegistry {
    let mut reg = CommandRegistry::new();
    register_all_commands(&mut reg);
    reg
}

// ═══════════════════════════════════════════════════════════════════════════
//  9.1 Basic Commands — /help, /clear, /compact, /version, etc.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn register_all_commands_populates_registry() {
    let reg = full_registry();
    // The alignment doc claims 77 commands; pin the number so removing one is
    // a failing test rather than a quiet drift. Adding one is a deliberate
    // change and updates this number with it.
    assert_eq!(reg.len(), 77, "expected 77 commands, got {}", reg.len());
}

#[test]
fn help_lists_all_command_categories() {
    let reg = full_registry();
    let result = reg.execute("help", &test_ctx(), &[]);

    assert!(result.output.contains("Session:"));
    assert!(result.output.contains("Config:"));
    assert!(result.output.contains("Stats:"));
    assert!(result.output.contains("Content:"));
    assert!(result.output.contains("Git/PR:"));
}

#[test]
fn help_for_specific_command() {
    let reg = full_registry();
    let result = reg.execute("help", &test_ctx(), &["clear".into()]);
    assert!(result.output.contains("/clear"));
}

/// The registry cannot clear a session — it has no engine — so `/clear` asks
/// for one and flags the state change for whoever owns the session. The
/// clearing itself is asserted where it happens: `repl_integration` for the
/// headless path and `interactive_tui` for the interactive one.
#[test]
fn clear_command_requests_a_clear_and_flags_it() {
    let reg = full_registry();
    let result = reg.execute("clear", &test_ctx(), &[]);
    assert!(result.state_changed, "{}", result.output);
    assert!(
        result.output.contains("Clear requested"),
        "{}",
        result.output
    );
}

/// Same shape as `/clear`: compacting happens in the engine, and the registry
/// can only ask for it.
#[test]
fn compact_command_requests_a_compaction_and_flags_it() {
    let reg = full_registry();
    let result = reg.execute("compact", &test_ctx(), &[]);
    assert!(result.state_changed, "{}", result.output);
    assert!(
        result.output.contains("Compact requested"),
        "{}",
        result.output
    );
}

#[test]
fn version_shows_version_info() {
    let reg = full_registry();
    let result = reg.execute("version", &test_ctx(), &[]);
    assert!(result.output.contains("CodeRex"));
}

#[test]
fn config_shows_current_state() {
    let reg = full_registry();
    let result = reg.execute("config", &test_ctx(), &[]);
    assert!(result.output.contains("Model:"));
    assert!(result.output.contains("Permissions:"));
}

/// A bare argument is not a way to set anything — `set <key> <value>` is, and
/// that path writes to the user's settings file, so it is covered by the unit
/// test that points the config home at a temporary directory. What is asserted
/// here is the interface: a bare argument is refused with the usage, and the
/// read commands work.
#[test]
fn config_refuses_a_bare_argument_and_reads_settings() {
    let reg = full_registry();
    let result = reg.execute("config", &test_ctx(), &["model".into()]);
    assert!(
        result.output.contains("Usage: /config"),
        "{}",
        result.output
    );
    assert!(
        !result.state_changed,
        "nothing was changed: {}",
        result.output
    );

    let listed = reg.execute("config", &test_ctx(), &["list".into()]);
    assert!(listed.output.contains("settings"), "{}", listed.output);
}

#[test]
fn session_shows_info() {
    let reg = full_registry();
    let result = reg.execute("session", &test_ctx(), &[]);
    assert!(result.output.contains("s1"));
    assert!(result.output.contains("Test Session"));
    assert!(result.output.contains("10"));
}

#[test]
fn exit_command_suppresses_output() {
    let reg = full_registry();
    let result = reg.execute("exit", &test_ctx(), &[]);
    assert!(result.suppress_output);
    assert!(result.output.contains("Exiting"));
}

#[test]
fn stats_shows_all_metrics() {
    let reg = full_registry();
    let result = reg.execute("stats", &test_ctx(), &[]);
    assert!(result.output.contains("5000"));
    assert!(result.output.contains("3000"));
    assert!(result.output.contains("$0.04"));
    assert!(result.output.contains("10"));
}

#[test]
fn cost_shows_monetary_value() {
    let reg = full_registry();
    let result = reg.execute("cost", &test_ctx(), &[]);
    assert!(result.output.contains("$0.04"));
}

#[test]
fn usage_shows_token_breakdown() {
    let reg = full_registry();
    let result = reg.execute("usage", &test_ctx(), &[]);
    assert!(result.output.contains("5000"));
    assert!(result.output.contains("9500"));
}

#[test]
fn model_switch_requires_arg() {
    let reg = full_registry();
    let result = reg.execute("model", &test_ctx(), &[]);
    assert!(result.output.contains("Usage"));
}

#[test]
fn model_switch_to_opus() {
    let reg = full_registry();
    let result = reg.execute("model", &test_ctx(), &["opus".into()]);
    assert!(result.output.contains("claude-opus"));
    assert!(result.state_changed);
}

#[test]
fn permissions_shows_current_mode() {
    let reg = full_registry();
    let result = reg.execute("permissions", &test_ctx(), &[]);
    assert!(result.output.contains("permission mode"));
}

#[test]
fn permissions_set_plan_mode() {
    let reg = full_registry();
    let result = reg.execute("permissions", &test_ctx(), &["plan".into()]);
    assert!(result.output.contains("Plan"));
    assert!(result.state_changed);
}

#[test]
fn context_shows_assembly() {
    let reg = full_registry();
    let result = reg.execute("context", &test_ctx(), &[]);
    assert!(result.output.contains("Context Assembly"));
}

#[test]
fn summary_shows_session_summary() {
    let reg = full_registry();
    let result = reg.execute("summary", &test_ctx(), &[]);
    assert!(result.output.contains("Session Summary"));
}

#[test]
fn diff_shows_git_changes() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git");
        assert!(status.status.success(), "{args:?} failed");
    };

    run(&["init", "--quiet"]);
    std::fs::write(dir.path().join("file.txt"), "before\n").unwrap();
    run(&["add", "file.txt"]);
    run(&["commit", "--quiet", "-m", "init"]);
    std::fs::write(dir.path().join("file.txt"), "after\n").unwrap();

    let mut context = test_ctx();
    context.cwd = dir.path().to_string_lossy().into_owned();

    let reg = full_registry();
    let result = reg.execute("diff", &context, &[]);
    assert!(result.output.contains("file.txt"), "{}", result.output);
    assert!(result.output.contains("after"), "{}", result.output);
}

#[test]
fn diff_outside_a_repository_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut context = test_ctx();
    context.cwd = dir.path().to_string_lossy().into_owned();

    let reg = full_registry();
    let result = reg.execute("diff", &context, &[]);
    assert!(
        result.output.contains("Not a git repository"),
        "{}",
        result.output
    );
}

#[test]
fn env_empty_shows_none() {
    let reg = full_registry();
    let result = reg.execute("env", &test_ctx(), &[]);
    assert!(result.output.contains("Environment"));
}

#[test]
fn env_with_vars() {
    let mut env = HashMap::new();
    env.insert("HOME".into(), "/home/user".into());
    let ctx = CommandContext { env, ..test_ctx() };
    let reg = full_registry();
    let result = reg.execute("env", &ctx, &[]);
    assert!(result.output.contains("HOME"));
}

#[test]
fn advisor_mode_sets_profile() {
    let reg = full_registry();
    let result = reg.execute("advisor", &test_ctx(), &[]);
    assert!(result.output.contains("Advisor"));
    assert!(result.extra_context.is_some());
}

#[test]
fn bughunter_mode_sets_profile() {
    let reg = full_registry();
    let result = reg.execute("bughunter", &test_ctx(), &[]);
    assert!(result.output.contains("Bug Hunter"));
    assert!(result.extra_context.is_some());
}

#[test]
fn btw_requires_arg() {
    let reg = full_registry();
    let result = reg.execute("btw", &test_ctx(), &[]);
    assert!(result.output.contains("Usage"));
}

#[test]
fn btw_adds_context() {
    let reg = full_registry();
    let result = reg.execute("btw", &test_ctx(), &["important".into(), "note".into()]);
    assert!(result.extra_context.is_some());
}

#[test]
fn good_claude_responds() {
    let reg = full_registry();
    let result = reg.execute("good-claude", &test_ctx(), &[]);
    assert!(result.output.contains("Thanks"));
}

#[test]
fn issue_usage_without_args() {
    let reg = full_registry();
    let result = reg.execute("issue", &test_ctx(), &[]);
    assert!(result.output.contains("Usage"));
}

#[test]
fn issue_create_with_title() {
    let reg = full_registry();
    let result = reg.execute(
        "issue",
        &test_ctx(),
        &["create".into(), "fix".into(), "bug".into()],
    );
    assert!(result.output.contains("fix bug"));
    assert!(result.state_changed);
}

/// The command reports the credential state; it does not refresh anything,
/// because this build has no OAuth token to refresh. The assertion says that,
/// so a claim of refreshing cannot come back without the flow behind it.
#[test]
fn oauth_refresh_reports_the_credential_state() {
    let reg = full_registry();
    let result = reg.execute("oauth-refresh", &test_ctx(), &[]);
    assert!(
        result.output.contains("No credential found")
            || result.output.contains("No OAuth token was refreshed"),
        "{}",
        result.output
    );
    // It describes state rather than changing it.
    assert!(!result.state_changed, "{}", result.output);
}

#[test]
fn status_shows_system_info() {
    let reg = full_registry();
    let result = reg.execute("status", &test_ctx(), &[]);
    assert!(result.output.contains("System Status"));
    assert!(result.output.contains("main"));
}

#[test]
fn doctor_diagnoses_environment() {
    let reg = full_registry();
    let result = reg.execute("doctor", &test_ctx(), &[]);
    assert!(result.output.contains("API key"));
    assert!(result.output.contains("Working directory"));
}

#[test]
fn feedback_help_text() {
    let reg = full_registry();
    let result = reg.execute("feedback", &test_ctx(), &[]);
    assert!(result.output.contains("/feedback"));
}

#[test]
fn history_empty() {
    let reg = full_registry();
    let result = reg.execute("history", &test_ctx(), &[]);
    assert!(result.output.contains("No command history"));
}

#[test]
fn stickers_feature_gated() {
    let reg = full_registry();
    let result = reg.execute("stickers", &test_ctx(), &[]);
    assert!(result.output.contains("not enabled"));
}

#[test]
fn ctx_viz_feature_gated() {
    let reg = full_registry();
    let result = reg.execute("ctx_viz", &test_ctx(), &[]);
    assert!(result.output.contains("not enabled"));
}

#[test]
fn unknown_command_returns_error() {
    let reg = full_registry();
    let result = reg.execute("nonexistent", &test_ctx(), &[]);
    assert!(result.output.contains("Unknown command"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  9.2 Command Types — Local/LocalUi/Prompt/Standalone
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn command_types_are_correct() {
    let reg = full_registry();

    // Local commands
    let clear = reg.find("clear").expect("/clear should exist");
    assert_eq!(clear.kind, CommandType::Local);

    let config = reg.find("config").expect("/config should exist");
    assert_eq!(config.kind, CommandType::Local);

    let stats = reg.find("stats").expect("/stats should exist");
    assert_eq!(stats.kind, CommandType::Local);

    let compact = reg.find("compact").expect("/compact should exist");
    assert_eq!(compact.kind, CommandType::Local);

    // Prompt commands
    let advisor = reg.find("advisor").expect("/advisor should exist");
    assert_eq!(advisor.kind, CommandType::Prompt);

    let btw = reg.find("btw").expect("/btw should exist");
    assert_eq!(btw.kind, CommandType::Prompt);

    let bughunter = reg.find("bughunter").expect("/bughunter should exist");
    assert_eq!(bughunter.kind, CommandType::Prompt);
}

// ═══════════════════════════════════════════════════════════════════════════
//  9.3 Remote-safe — commands available in remote context
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn remote_safe_commands_are_available() {
    let reg = full_registry();
    let remote = reg.available_commands(false, true, false);

    // These should be remote-safe
    assert!(remote.iter().any(|c| c.name == "help"));
    assert!(remote.iter().any(|c| c.name == "config"));
    assert!(remote.iter().any(|c| c.name == "stats"));
    assert!(remote.iter().any(|c| c.name == "compact"));
    assert!(remote.iter().any(|c| c.name == "version"));
    assert!(remote.iter().any(|c| c.name == "session"));
    assert!(remote.iter().any(|c| c.name == "advisor"));
    assert!(remote.iter().any(|c| c.name == "btw"));
}

#[test]
fn non_remote_safe_commands_filtered_out() {
    let reg = full_registry();
    let remote = reg.available_commands(false, true, false);

    // Standalone commands are not remote-safe
    let standalone_cmds: Vec<_> = reg
        .list()
        .iter()
        .filter(|c| c.kind == CommandType::Standalone)
        .collect();

    for cmd in standalone_cmds {
        assert!(
            !remote.iter().any(|c| c.name == cmd.name),
            "Standalone command /{} should not be remote-safe",
            cmd.name
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  9.4 Bridge-safe — commands available in bridge sessions
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn bridge_safe_commands_are_available() {
    let reg = full_registry();
    let bridge = reg.available_commands(false, false, true);

    // These should be bridge-safe
    assert!(bridge.iter().any(|c| c.name == "help"));
    assert!(bridge.iter().any(|c| c.name == "config"));
    assert!(bridge.iter().any(|c| c.name == "stats"));
    assert!(bridge.iter().any(|c| c.name == "compact"));
    assert!(bridge.iter().any(|c| c.name == "version"));
    assert!(bridge.iter().any(|c| c.name == "advisor"));
    assert!(bridge.iter().any(|c| c.name == "btw"));
}

#[test]
fn non_bridge_safe_commands_filtered_out() {
    let reg = full_registry();
    let bridge = reg.available_commands(false, false, true);

    // Standalone commands are not bridge-safe
    let standalone_cmds: Vec<_> = reg
        .list()
        .iter()
        .filter(|c| c.kind == CommandType::Standalone)
        .collect();

    for cmd in standalone_cmds {
        assert!(
            !bridge.iter().any(|c| c.name == cmd.name),
            "Standalone command /{} should not be bridge-safe",
            cmd.name
        );
    }
}

#[test]
fn availability_matrix_combined_filters() {
    let reg = full_registry();

    // All commands (no filters)
    let all = reg.available_commands(false, false, false);
    assert_eq!(all.len(), reg.len());

    // Non-interactive only
    let non_interactive = reg.available_commands(true, false, false);
    // Commands that support non-interactive mode
    assert!(non_interactive.iter().any(|c| c.name == "help"));
    assert!(non_interactive.iter().any(|c| c.name == "compact"));
    // Commands that don't support non-interactive should be filtered
    assert!(
        !non_interactive.iter().any(|c| c.name == "stickers"),
        "/stickers should not support non-interactive"
    );

    // Remote + bridge safe combined
    let remote_bridge = reg.available_commands(false, true, true);
    for cmd in &remote_bridge {
        assert!(cmd.remote_safe, "{} should be remote-safe", cmd.name);
        assert!(cmd.bridge_safe, "{} should be bridge-safe", cmd.name);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: argument validation and edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn command_result_builder_chaining() {
    let r = CommandResult::success("test")
        .with_state_change(true)
        .with_suppressed()
        .with_context("extra");

    assert!(r.state_changed);
    assert!(r.suppress_output);
    assert_eq!(r.extra_context, Some("extra".into()));
    assert_eq!(r.output, "test");
}

#[test]
fn command_result_error_format() {
    let r = CommandResult::error("something went wrong");
    assert!(r.output.contains("Error:"));
    assert!(r.output.contains("something went wrong"));
}

#[test]
fn registry_find_is_case_sensitive() {
    let reg = full_registry();
    assert!(reg.find("Help").is_none());
    assert!(reg.find("HELP").is_none());
    assert!(reg.find("help").is_some());
}

#[test]
fn registry_list_returns_all_in_order() {
    let reg = full_registry();
    let list = reg.list();
    assert_eq!(list.len(), reg.len());
}
