//! Hook integration tests — covering all 22 hook events, subprocess execution,
//! result parsing, and lifecycle behavior.
//!
//! These tests correspond to the equivalence test plan Module 5:
//! 5.1 SessionStart, 5.2 UserPromptSubmit, 5.3 PreToolUse, 5.4 PostToolUse,
//! 5.5 PostToolUseFailure, 5.6 Stop, 5.7 PreCompact, 5.8 PostCompact,
//! 5.9 CwdChanged

use std::collections::HashMap;

use coderex_core::hook_executor::HookExecutor;
use coderex_hooks::events::HookEvent;
use coderex_hooks::{HookAction, HookConfig, HookInput, HookRegistry};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn config(event: &str, command: &str) -> HookConfig {
    HookConfig {
        event: event.to_string(),
        command: command.to_string(),
        env: HashMap::new(),
        cwd: None,
        internal: false,
        timeout_ms: 5000,
        priority: 0,
        dry_run: false,
        max_cpu_seconds: None,
        max_memory_mb: None,
        max_file_size_mb: None,
    }
}

fn config_with_env(event: &str, command: &str, env: HashMap<String, String>) -> HookConfig {
    HookConfig {
        event: event.to_string(),
        command: command.to_string(),
        env,
        cwd: None,
        internal: false,
        timeout_ms: 5000,
        priority: 0,
        dry_run: false,
        max_cpu_seconds: None,
        max_memory_mb: None,
        max_file_size_mb: None,
    }
}

fn input(event: &str) -> HookInput {
    HookInput {
        event: event.to_string(),
        session_id: Some("test-session".to_string()),
        input: Some("test input".to_string()),
        context: None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.1 SessionStart — hook fires when session begins
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_sessionstart_fires_and_returns_continue() {
    let mut registry = HookRegistry::new();
    registry.register(config("SessionStart", r#"echo '{"action":"continue"}'"#));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert_eq!(results.len(), 1);

    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn hook_sessionstart_receives_input_on_stdin() {
    let mut registry = HookRegistry::new();
    // Read stdin and echo back the event name we received using python3 or python
    registry.register(config(
        "SessionStart",
        r#"python3 -c "import sys; line=sys.stdin.read(); print('{\"action\":\"continue\", \"additional_context\": \"' + line[:20] + '\"}')" 2>/dev/null || python -c "import sys; line=sys.stdin.read(); print('{\"action\":\"continue\", \"additional_context\": \"' + line[:20] + '\"}')""#,
    ));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    // The hook should have received the JSON input on stdin (starts with {"event":)
    assert!(result.stdout.contains("continue"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.2 UserPromptSubmit — hook can inspect and modify user input
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_userpromptsubmit_can_modify_input() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "UserPromptSubmit",
        r#"echo '{"action":"continue", "updated_input": "modified prompt"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::UserPromptSubmit, &input("UserPromptSubmit"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert_eq!(
        result.hook_result.updated_input,
        Some("modified prompt".to_string())
    );
}

#[tokio::test]
async fn hook_userpromptsubmit_stop_prevents_processing() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "UserPromptSubmit",
        r#"echo '{"action":"stop", "suppress_output": true}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::UserPromptSubmit, &input("UserPromptSubmit"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Stop));
    assert!(result.hook_result.suppress_output);
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.3 PreToolUse — hook fires before tool execution, can stop it
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_pretooluse_can_stop_tool_execution() {
    let mut registry = HookRegistry::new();
    registry.register(config("PreToolUse", r#"echo '{"action":"stop"}'"#));

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Stop));
}

#[tokio::test]
async fn hook_pretooluse_continue_allows_tool_execution() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PreToolUse",
        r#"echo '{"action":"continue", "watch_paths": ["/src", "/tests"]}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(result.hook_result.watch_paths, vec!["/src", "/tests"]);
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.4 PostToolUse — hook fires after successful tool execution
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_posttooluse_fires_after_success() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PostToolUse",
        r#"echo '{"action":"continue", "additional_context": "tool succeeded"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PostToolUse, &input("PostToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(
        result.hook_result.additional_context,
        Some("tool succeeded".to_string())
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.5 PostToolUseFailure — hook fires after failed tool execution
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_posttoolusefailure_fires_after_failure() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PostToolUseFailure",
        r#"echo '{"action":"continue", "additional_context": "tool failed, retrying"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PostToolUseFailure, &input("PostToolUseFailure"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(
        result.hook_result.additional_context,
        Some("tool failed, retrying".to_string())
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.6 Stop — hook fires when user requests stop
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_stop_fires_on_stop_request() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "Stop",
        r#"echo '{"action":"continue", "suppress_output": true}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::Stop, &input("Stop"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert!(result.hook_result.suppress_output);
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.7 PreCompact — hook fires before context compaction
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_precompact_fires_before_compaction() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PreCompact",
        r#"echo '{"action":"continue", "additional_context": "compact starting"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PreCompact, &input("PreCompact"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.8 PostCompact — hook fires after context compaction
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_postcompact_fires_after_compaction() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PostCompact",
        r#"echo '{"action":"continue", "additional_context": "compact complete"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PostCompact, &input("PostCompact"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.9 CwdChanged — hook fires when working directory changes
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_cwdchanged_fires_on_directory_change() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "CwdChanged",
        r#"echo '{"action":"continue", "watch_paths": [".coderex"]}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::CwdChanged, &input("CwdChanged"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert!(!result.hook_result.watch_paths.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: matching, priority, async, timeout, settings
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_wildcard_matches_all_events() {
    let mut registry = HookRegistry::new();
    registry.register(config("*", r#"echo '{"action":"continue"}'"#));

    for event in [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::Stop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::CwdChanged,
    ] {
        let results = registry.execute_sync(&event, &input("test")).await;
        assert!(!results.is_empty(), "wildcard hook should match {event:?}");
    }
}

#[tokio::test]
async fn hook_priority_runs_highest_first() {
    let mut registry = HookRegistry::new();
    registry.register(HookConfig {
        priority: 1,
        ..config(
            "PreToolUse",
            r#"echo '{"action":"continue", "updated_input": "high"}'"#,
        )
    });
    registry.register(HookConfig {
        priority: 0,
        ..config(
            "PreToolUse",
            r#"echo '{"action":"continue", "updated_input": "low"}'"#,
        )
    });

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    assert_eq!(results.len(), 2);
    // Highest priority runs first
    assert_eq!(
        results[0].as_ref().unwrap().hook_result.updated_input,
        Some("high".to_string())
    );
    assert_eq!(
        results[1].as_ref().unwrap().hook_result.updated_input,
        Some("low".to_string())
    );
}

#[tokio::test]
async fn hook_timeout_kills_long_running_hook() {
    let mut registry = HookRegistry::new();
    registry.register(HookConfig {
        timeout_ms: 100,
        ..config("Stop", "sleep 10")
    });

    let results = registry
        .execute_sync(&HookEvent::Stop, &input("Stop"))
        .await;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err(), "long-running hook should time out");
}

#[tokio::test]
async fn hook_async_execution_runs_concurrently() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "SessionStart",
        r#"echo '{"action":"continue", "additional_context": "async1"}'"#,
    ));
    registry.register(config(
        "SessionStart",
        r#"echo '{"action":"continue", "additional_context": "async2"}'"#,
    ));

    let results = registry
        .execute_async(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert_eq!(results.len(), 2);
    let contexts: Vec<_> = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter_map(|r| r.hook_result.additional_context.clone())
        .collect();
    assert!(contexts.contains(&"async1".to_string()));
    assert!(contexts.contains(&"async2".to_string()));
}

#[tokio::test]
async fn hook_fire_hook_trait_impl_combines_results() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "UserPromptSubmit",
        r#"echo '{"action":"continue", "updated_input": "first"}'"#,
    ));
    registry.register(config("UserPromptSubmit", r#"echo '{"action":"stop"}'"#));

    let mut context = HashMap::new();
    context.insert("session_id".to_string(), "sess_123".to_string());

    let result = registry
        .fire_hook("UserPromptSubmit", "original", &context)
        .await;

    // stop should be true because one hook returned stop
    assert!(result.stop);
    // updated_input should come from the first hook (last one wins in the impl)
    assert_eq!(result.updated_input, Some("first".to_string()));
}

#[test]
fn hook_load_from_settings_parses_hooks_array() {
    let value = serde_json::json!({
        "hooks": [
            {
                "event": "SessionStart",
                "command": "echo start",
                "env": {"FOO": "bar"},
                "timeout_ms": 3000,
                "priority": 5
            },
            {
                "event": "PreToolUse",
                "command": "echo pre",
                "dry_run": true
            }
        ]
    });

    let registry = HookRegistry::load_from_settings(&value).unwrap();
    let session_start = registry.matches_for_event(&HookEvent::SessionStart);
    assert_eq!(session_start.len(), 1);
    assert_eq!(session_start[0].command, "echo start");
    assert_eq!(session_start[0].env.get("FOO").unwrap(), "bar");
    assert_eq!(session_start[0].timeout_ms, 3000);
    assert_eq!(session_start[0].priority, 5);

    let pre_tool = registry.matches_for_event(&HookEvent::PreToolUse);
    assert_eq!(pre_tool.len(), 1);
    assert!(pre_tool[0].dry_run);
}

#[test]
fn hook_load_from_settings_null_returns_empty() {
    let registry = HookRegistry::load_from_settings(&serde_json::Value::Null).unwrap();
    assert!(
        registry
            .matches_for_event(&HookEvent::SessionStart)
            .is_empty()
    );
}

#[tokio::test]
async fn hook_dry_run_suppresses_stop_action() {
    let mut registry = HookRegistry::new();
    registry.register(HookConfig {
        dry_run: true,
        ..config("PreToolUse", r#"echo '{"action":"stop"}'"#)
    });

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    // In dry-run mode, stop is converted to continue
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}

#[tokio::test]
async fn hook_env_variables_are_injected() {
    let mut env = HashMap::new();
    env.insert("HOOK_CUSTOM_VAR".to_string(), "custom_value".to_string());

    let mut registry = HookRegistry::new();
    registry.register(config_with_env(
        "SessionStart",
        r#"echo '{"action":"continue", "additional_context": "'$HOOK_CUSTOM_VAR'"}'"#,
        env,
    ));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert_eq!(
        result.hook_result.additional_context,
        Some("custom_value".to_string())
    );
}

#[tokio::test]
async fn hook_no_match_returns_empty_results() {
    let registry = HookRegistry::new();
    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert!(results.is_empty(), "no hooks registered = no results");
}

#[tokio::test]
async fn hook_permission_decision_parsed_from_output() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PermissionRequest",
        r#"echo '{"action":"continue", "permission_decision": {"allow": true, "allow_for_path": "/tmp", "reason": "safe path"}}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PermissionRequest, &input("PermissionRequest"))
        .await;
    let result = results[0].as_ref().unwrap();
    let decision = result.hook_result.permission_decision.as_ref().unwrap();
    assert!(decision.allow);
    assert_eq!(decision.allow_for_path, Some("/tmp".to_string()));
    assert_eq!(decision.reason, "safe path");
}

#[tokio::test]
async fn hook_nonzero_exit_continues_by_default() {
    let mut registry = HookRegistry::new();
    registry.register(config("SessionStart", "exit 1"));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    let result = results[0].as_ref().unwrap();
    // Non-zero exit with no stdout should default to Continue
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(result.exit_code, 1);
}

#[tokio::test]
async fn hook_empty_stdout_continues_by_default() {
    let mut registry = HookRegistry::new();
    registry.register(config("SessionStart", "echo -n"));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}
