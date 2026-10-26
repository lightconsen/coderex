//! Permissions integration tests — covering PermissionEngine with all modes,
//! hooks, confirmation bus, and path-based rules.
//!
//! These tests correspond to the equivalence test plan Module 3:
//! 3.1 Default read, 3.2 Default write, 3.3 Default dangerous shell,
//! 3.4 AcceptEdits, 3.5 BypassPermissions, 3.6 DontAsk, 3.7 Plan, 3.8 Path Allowlist

use std::collections::HashMap;
use std::path::Path;

use coderex_core::permission_engine::{
    AutoApproveHook, BackgroundAgentDenyHook, NoOpPermissionHook, PermissionEngine,
};
use coderex_core::permissions::{
    PermissionAction, PermissionChecker, PermissionMode, PermissionResult, PermissionRule,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn make_engine(mode: PermissionMode) -> PermissionEngine {
    let checker = PermissionChecker::new(mode);
    PermissionEngine::new(checker)
}

fn ctx() -> HashMap<String, String> {
    HashMap::new()
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.1 Default mode — read operations auto-allowed
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn default_mode_read_auto_allowed() {
    let mut engine = make_engine(PermissionMode::Default);
    let (_req, result) = engine
        .evaluate("Read", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        result.is_allowed(),
        "Read should be auto-allowed in Default mode"
    );
}

#[tokio::test]
async fn default_mode_grep_auto_allowed() {
    let mut engine = make_engine(PermissionMode::Default);
    let (_req, result) = engine
        .evaluate("Grep", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        result.is_allowed(),
        "Grep should be auto-allowed in Default mode"
    );
}

#[tokio::test]
async fn default_mode_glob_auto_allowed() {
    let mut engine = make_engine(PermissionMode::Default);
    let (_req, result) = engine
        .evaluate("Glob", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        result.is_allowed(),
        "Glob should be auto-allowed in Default mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.2 Default mode — write operations require approval
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn default_mode_write_requires_approval() {
    let mut engine = make_engine(PermissionMode::Default);
    let (_req, result) = engine
        .evaluate("Write", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Ask { .. }),
        "Write should require approval in Default mode, got: {result:?}"
    );
}

#[tokio::test]
async fn default_mode_edit_requires_approval() {
    let mut engine = make_engine(PermissionMode::Default);
    let (_req, result) = engine
        .evaluate("Edit", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Ask { .. }),
        "Edit should require approval in Default mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.3 Default mode — dangerous shell requires approval
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn default_mode_bash_requires_approval() {
    let mut engine = make_engine(PermissionMode::Default);
    let (_req, result) = engine
        .evaluate("Bash", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Ask { .. }),
        "Bash should require approval in Default mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.4 AcceptEdits mode
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn accept_edits_mode_allows_read() {
    let mut engine = make_engine(PermissionMode::AcceptEdits);
    let (_req, result) = engine
        .evaluate("Read", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(result.is_allowed());
}

#[tokio::test]
async fn accept_edits_mode_auto_allows_write() {
    let mut engine = make_engine(PermissionMode::AcceptEdits);
    let (_req, result) = engine
        .evaluate("Write", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        result.is_allowed(),
        "Write should be allowed in AcceptEdits mode"
    );
}

#[tokio::test]
async fn accept_edits_mode_shell_still_requires_approval() {
    let mut engine = make_engine(PermissionMode::AcceptEdits);
    let (_req, result) = engine
        .evaluate("Bash", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Ask { .. }),
        "Bash should still require approval in AcceptEdits mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.5 BypassPermissions mode — all auto-allowed
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn bypass_permissions_allows_all_tools() {
    let mut engine = make_engine(PermissionMode::BypassPermissions);

    for tool in ["Read", "Write", "Edit", "Bash", "Glob", "Grep"] {
        let (_req, result) = engine
            .evaluate(tool, "{}".into(), None, ctx())
            .await
            .unwrap();
        assert!(
            matches!(result, PermissionResult::Allow),
            "{tool} should be allowed in BypassPermissions mode"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.6 DontAsk mode — auto-deny
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn dontask_mode_denies_all() {
    let mut engine = make_engine(PermissionMode::DontAsk);

    for tool in ["Read", "Write", "Edit", "Bash", "Glob", "Grep"] {
        let (_req, result) = engine
            .evaluate(tool, "{}".into(), None, ctx())
            .await
            .unwrap();
        assert!(
            matches!(result, PermissionResult::Deny { .. }),
            "{tool} should be denied in DontAsk mode"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.7 Plan mode — read-only only
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn plan_mode_allows_read_only_tools() {
    let mut engine = make_engine(PermissionMode::Plan);

    // Note: PermissionChecker recognizes these as read-only.
    // WebSearch/WebFetch are not in the checker's read-only list
    // because their tool names ("WebSearch"/"WebFetch") don't match
    // the lowercase snake_case patterns ("web_search"/"web_fetch").
    for tool in ["Read", "Glob", "Grep"] {
        let (_req, result) = engine
            .evaluate(tool, "{}".into(), None, ctx())
            .await
            .unwrap();
        assert!(result.is_allowed(), "{tool} should be allowed in Plan mode");
    }
}

#[tokio::test]
async fn plan_mode_denies_mutating_tools() {
    let mut engine = make_engine(PermissionMode::Plan);

    for tool in ["Write", "Edit", "Bash", "NotebookEdit"] {
        let (_req, result) = engine
            .evaluate(tool, "{}".into(), None, ctx())
            .await
            .unwrap();
        assert!(
            matches!(result, PermissionResult::Deny { .. }),
            "{tool} should be denied in Plan mode"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3.8 Path Allowlist
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn path_allowlist_allows_matching_paths() {
    let mut checker = PermissionChecker::new(PermissionMode::Default);
    checker
        .add_rule(PermissionRule {
            tool_name: "Write".into(),
            action: PermissionAction::Allow,
            paths: vec!["/tmp/**".into()],
            description: "allow writes to /tmp".into(),
        })
        .unwrap();

    let mut engine = PermissionEngine::new(checker);
    let (_req, result) = engine
        .evaluate(
            "Write",
            "{}".into(),
            Some(Path::new("/tmp/foo.txt").to_path_buf()),
            ctx(),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            result,
            PermissionResult::Allow | PermissionResult::AllowForPath { .. }
        ),
        "Write to /tmp should be allowed, got: {result:?}"
    );
}

#[tokio::test]
async fn path_allowlist_denies_non_matching_paths() {
    let mut checker = PermissionChecker::new(PermissionMode::Default);
    checker
        .add_rule(PermissionRule {
            tool_name: "Write".into(),
            action: PermissionAction::Allow,
            paths: vec!["/tmp/**".into()],
            description: "allow writes to /tmp".into(),
        })
        .unwrap();

    let mut engine = PermissionEngine::new(checker);
    let (_req, result) = engine
        .evaluate(
            "Write",
            "{}".into(),
            Some(Path::new("/etc/passwd").to_path_buf()),
            ctx(),
        )
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Ask { .. }),
        "Write to /etc should require approval, got: {result:?}"
    );
}

#[tokio::test]
async fn path_deny_rule_has_priority_over_allow() {
    let mut checker = PermissionChecker::new(PermissionMode::Default);
    // Deny rule first
    checker
        .add_rule(PermissionRule {
            tool_name: "Bash".into(),
            action: PermissionAction::Deny,
            paths: vec!["/etc/**".into()],
            description: "no bash in /etc".into(),
        })
        .unwrap();
    // Allow rule second
    checker
        .add_rule(PermissionRule {
            tool_name: "Bash".into(),
            action: PermissionAction::Allow,
            paths: vec!["/etc/hosts".into()],
            description: String::new(),
        })
        .unwrap();

    let mut engine = PermissionEngine::new(checker);
    let (_req, result) = engine
        .evaluate(
            "Bash",
            "{}".into(),
            Some(Path::new("/etc/hosts").to_path_buf()),
            ctx(),
        )
        .await
        .unwrap();
    // Deny rules are checked first, so this should be denied
    assert!(
        matches!(result, PermissionResult::Deny { .. }),
        "Deny rule should have priority"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Permission engine with hooks
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auto_approve_hook_short_circuits_checker() {
    let checker = PermissionChecker::new(PermissionMode::Default);
    let mut engine = PermissionEngine::new(checker);
    engine.add_hook(std::sync::Arc::new(AutoApproveHook::for_tools(vec![
        "Bash".into(),
    ])));

    let (_req, result) = engine
        .evaluate("Bash", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Allow),
        "AutoApproveHook should short-circuit and allow Bash"
    );
}

#[tokio::test]
async fn noop_hook_allows_fallthrough_to_checker() {
    let checker = PermissionChecker::new(PermissionMode::Default);
    let mut engine = PermissionEngine::new(checker);
    engine.add_hook(std::sync::Arc::new(NoOpPermissionHook));

    let (_req, result) = engine
        .evaluate("Bash", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Ask { .. }),
        "NoOpHook should fall through to checker"
    );
}

#[tokio::test]
async fn background_agent_deny_hook_blocks_in_background_mode() {
    let checker = PermissionChecker::new(PermissionMode::BypassPermissions);
    let mut engine = PermissionEngine::new(checker);
    engine.add_hook(std::sync::Arc::new(BackgroundAgentDenyHook));

    let mut context = ctx();
    context.insert("execution_mode".into(), "background".into());

    let (_req, result) = engine
        .evaluate("Read", "{}".into(), None, context)
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Deny { .. }),
        "BackgroundAgentDenyHook should deny in background mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Permission request structure
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn permission_request_contains_correct_fields() {
    let mut engine = make_engine(PermissionMode::Default);
    let (request, result) = engine
        .evaluate(
            "Write",
            "{\"file\":\"test.txt\"}".into(),
            Some(Path::new("/tmp/test.txt").to_path_buf()),
            ctx(),
        )
        .await
        .unwrap();

    assert_eq!(request.tool_name, "Write");
    assert!(request.arguments.contains("test.txt"));
    assert_eq!(request.path, Some(Path::new("/tmp/test.txt").to_path_buf()));
    assert!(matches!(result, PermissionResult::Ask { .. }));
    assert!(!request.id.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Session approval caching via PermissionEngine
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_approval_cached_across_evaluations() {
    let mut checker = PermissionChecker::new(PermissionMode::Default);
    checker.record_session_approval("Bash".into(), true);

    let mut engine = PermissionEngine::new(checker);
    let (_req, result) = engine
        .evaluate("Bash", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Allow),
        "Bash should be allowed via session cache"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Auto mode
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn auto_mode_safe_tools_allowed() {
    let mut engine = make_engine(PermissionMode::Auto);
    let (_req, result) = engine
        .evaluate("Read", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(result.is_allowed(), "Read should be allowed in Auto mode");
}

#[tokio::test]
async fn auto_mode_dangerous_tools_require_approval() {
    let mut engine = make_engine(PermissionMode::Auto);
    let (_req, result) = engine
        .evaluate("Bash", "{}".into(), None, ctx())
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Ask { .. }),
        "Bash should require approval in Auto mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Settings-style rules — parsed specs and explicit subjects
// ═══════════════════════════════════════════════════════════════════════════

use coderex_core::permissions::{PermissionRuleSpec, RuleCompileContext};

fn rule(raw: &str, action: PermissionAction) -> PermissionRuleSpec {
    PermissionRuleSpec::parse(raw, action).unwrap_or_else(|error| panic!("{raw}: {error}"))
}

fn engine_with_rules(specs: &[PermissionRuleSpec], cwd: &Path) -> PermissionEngine {
    let mut checker = PermissionChecker::new(PermissionMode::Default);
    checker
        .add_specs(specs, &RuleCompileContext { cwd, home: None })
        .unwrap();
    PermissionEngine::new(checker)
}

#[tokio::test]
async fn settings_style_bash_rules_match_the_command_subject() {
    let mut engine = engine_with_rules(
        &[
            rule("Bash(rm:*)", PermissionAction::Deny),
            rule("Bash(ls:*)", PermissionAction::Allow),
        ],
        Path::new("/proj"),
    );

    let (_req, denied) = engine
        .evaluate_with_subjects(
            "Bash",
            r#"{"command":"rm -rf /tmp/x"}"#.into(),
            None,
            ctx(),
            vec!["rm -rf /tmp/x".into()],
        )
        .await
        .unwrap();
    assert!(
        matches!(denied, PermissionResult::Deny { .. }),
        "{denied:?}"
    );

    let (_req, allowed) = engine
        .evaluate_with_subjects(
            "Bash",
            r#"{"command":"ls -la"}"#.into(),
            None,
            ctx(),
            vec!["ls -la".into()],
        )
        .await
        .unwrap();
    assert!(allowed.is_allowed(), "{allowed:?}");

    let (_req, unrelated) = engine
        .evaluate_with_subjects(
            "Bash",
            r#"{"command":"git status"}"#.into(),
            None,
            ctx(),
            vec!["git status".into()],
        )
        .await
        .unwrap();
    assert!(
        matches!(unrelated, PermissionResult::Ask { .. }),
        "an unmatched dangerous command should ask: {unrelated:?}"
    );
}

#[tokio::test]
async fn an_ask_rule_beats_an_allow_rule_in_the_engine() {
    let mut engine = engine_with_rules(
        &[
            rule("Bash(echo:*)", PermissionAction::Allow),
            rule("Bash(echo hi:*)", PermissionAction::Ask),
        ],
        Path::new("/proj"),
    );
    let (_req, result) = engine
        .evaluate_with_subjects(
            "Bash",
            r#"{"command":"echo hi"}"#.into(),
            None,
            ctx(),
            vec!["echo hi".into()],
        )
        .await
        .unwrap();
    assert!(matches!(result, PermissionResult::Ask { .. }), "{result:?}");
}

#[tokio::test]
async fn path_rules_match_raw_and_absolute_subjects() {
    let mut engine = engine_with_rules(
        &[rule("Read(./.env)", PermissionAction::Deny)],
        Path::new("/proj"),
    );
    for subject in [".env", "/proj/.env"] {
        let (_req, result) = engine
            .evaluate_with_subjects(
                "Read",
                r#"{"file_path":".env"}"#.into(),
                None,
                ctx(),
                vec![subject.into()],
            )
            .await
            .unwrap();
        assert!(
            matches!(result, PermissionResult::Deny { .. }),
            "{subject} should match: {result:?}"
        );
    }
}
