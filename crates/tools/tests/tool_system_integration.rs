//! Tool System integration tests — covering registry, executor, dispatcher,
//! cross-tool workflows, concurrency, caching, and sandbox policy.
//!
//! These tests correspond to the equivalence test plan Module 2:
//! 2.1-2.16 baseline tool verification, plus cross-cutting concerns.

use std::path::PathBuf;
use std::sync::Arc;

use coderex_core::tool_executor::ToolExecutor;
use coderex_tools::{
    ExecutionConfig, ExecutionContext, RegistryToolDispatcher, ToolCacheConfig,
    ToolExecutor as ToolsExecutor, ToolRegistry, register_builtin_tools,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn test_registry() -> Arc<ToolRegistry> {
    let mut reg = ToolRegistry::new();
    register_builtin_tools(&mut reg);
    Arc::new(reg)
}

fn test_dispatcher(cwd: PathBuf) -> RegistryToolDispatcher {
    let registry = test_registry();
    let mut config = ExecutionConfig::default();
    config.sandbox_enabled = false;
    let executor = Arc::new(ToolsExecutor::new(config));
    let mut dispatcher =
        RegistryToolDispatcher::new(registry, executor, ExecutionContext::Normal, cwd);
    dispatcher.set_sandbox_enabled(false);
    dispatcher
}

const fn tool_args(args: serde_json::Value) -> serde_json::Value {
    args
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.1 Read Tool — via dispatcher
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn read_tool_via_dispatcher_reads_file() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("test.txt");
    std::fs::write(&file, "hello world\nline two").unwrap();

    let dispatcher = test_dispatcher(tmp.path().to_path_buf());
    let outcome = dispatcher
        .execute(
            "tc-1",
            "Read",
            &tool_args(serde_json::json!({"file_path": "test.txt"})),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(outcome.result.output.contains("hello world"));
}

#[tokio::test]
async fn read_tool_missing_file_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());
    let outcome = dispatcher
        .execute(
            "tc-2",
            "Read",
            &tool_args(serde_json::json!({"file_path": "no_such_file.txt"})),
        )
        .await;

    assert!(outcome.result.is_error);
    assert!(
        outcome.result.output.contains("not found") || outcome.result.output.contains("failed")
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.2 Write Tool — via dispatcher
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn write_tool_via_dispatcher_creates_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    let outcome = dispatcher
        .execute(
            "tc-3",
            "Write",
            &tool_args(serde_json::json!({
                "file_path": "new.txt",
                "content": "hello"
            })),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(tmp.path().join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn write_tool_creates_parent_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    let outcome = dispatcher
        .execute(
            "tc-4",
            "Write",
            &tool_args(serde_json::json!({
                "file_path": "a/b/c/deep.txt",
                "content": "nested"
            })),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(tmp.path().join("a/b/c/deep.txt").exists());
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.3 Edit Tool — via dispatcher
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn edit_tool_via_dispatcher_replaces_string() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("edit.txt"), "hello world").unwrap();

    let dispatcher = test_dispatcher(tmp.path().to_path_buf());
    let outcome = dispatcher
        .execute(
            "tc-5",
            "Edit",
            &tool_args(serde_json::json!({
                "file_path": "edit.txt",
                "old_string": "hello",
                "new_string": "hi",
                "replace_all": false
            })),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("edit.txt")).unwrap(),
        "hi world"
    );
}

#[tokio::test]
async fn edit_tool_non_unique_string_errors() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("dup.txt"), "foo foo foo").unwrap();

    let dispatcher = test_dispatcher(tmp.path().to_path_buf());
    let outcome = dispatcher
        .execute(
            "tc-6",
            "Edit",
            &tool_args(serde_json::json!({
                "file_path": "dup.txt",
                "old_string": "foo",
                "new_string": "bar",
                "replace_all": false
            })),
        )
        .await;

    assert!(outcome.result.is_error);
    assert!(
        outcome.result.output.contains("replace_all") || outcome.result.output.contains("3 times")
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.4 Bash Tool — via dispatcher
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn bash_tool_via_dispatcher_runs_command() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    let outcome = dispatcher
        .execute(
            "tc-7",
            "Bash",
            &tool_args(serde_json::json!({"command": "echo hello"})),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(outcome.result.output.contains("hello"));
    assert!(outcome.result.output.contains("Exit code: 0"));
}

#[tokio::test]
async fn bash_tool_exit_code_captured() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    let outcome = dispatcher
        .execute(
            "tc-8",
            "Bash",
            &tool_args(serde_json::json!({"command": "exit 7"})),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(outcome.result.output.contains("Exit code: 7"));
}

#[tokio::test]
async fn bash_tool_stderr_included() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    let outcome = dispatcher
        .execute(
            "tc-9",
            "Bash",
            &tool_args(serde_json::json!({"command": "echo err_msg >&2"})),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(outcome.result.output.contains("err_msg"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.5 Glob Tool — via dispatcher
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn glob_tool_via_dispatcher_matches_files() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "").unwrap();
    std::fs::write(tmp.path().join("b.rs"), "").unwrap();
    std::fs::write(tmp.path().join("readme.md"), "").unwrap();

    let dispatcher = test_dispatcher(tmp.path().to_path_buf());
    let outcome = dispatcher
        .execute(
            "tc-10",
            "Glob",
            &tool_args(serde_json::json!({
                "pattern": "*.rs",
                "path": tmp.path().to_str().unwrap()
            })),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(outcome.result.output.contains("a.rs"));
    assert!(outcome.result.output.contains("b.rs"));
    assert!(!outcome.result.output.contains("readme.md"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.6 Grep Tool — via dispatcher
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn grep_tool_via_dispatcher_finds_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("src.txt"),
        "hello world\nfoo bar\nhello again",
    )
    .unwrap();

    let dispatcher = test_dispatcher(tmp.path().to_path_buf());
    let outcome = dispatcher
        .execute(
            "tc-11",
            "Grep",
            &tool_args(serde_json::json!({
                "pattern": "hello",
                "path": tmp.path().to_str().unwrap()
            })),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(outcome.result.output.contains("hello world"));
    assert!(outcome.result.output.contains("hello again"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.7 Cross-tool workflow: Write then Read
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cross_tool_write_then_read() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    // Write
    let write_outcome = dispatcher
        .execute(
            "tc-12",
            "Write",
            &tool_args(serde_json::json!({
                "file_path": "workflow.txt",
                "content": "workflow content"
            })),
        )
        .await;
    assert!(!write_outcome.result.is_error);

    // Read back
    let read_outcome = dispatcher
        .execute(
            "tc-13",
            "Read",
            &tool_args(serde_json::json!({"file_path": "workflow.txt"})),
        )
        .await;
    assert!(!read_outcome.result.is_error);
    assert!(read_outcome.result.output.contains("workflow content"));
}

#[tokio::test]
async fn cross_tool_write_edit_read() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    // Write
    dispatcher
        .execute(
            "tc-14a",
            "Write",
            &tool_args(serde_json::json!({
                "file_path": "edit_chain.txt",
                "content": "original text"
            })),
        )
        .await;

    // Edit
    dispatcher
        .execute(
            "tc-14b",
            "Edit",
            &tool_args(serde_json::json!({
                "file_path": "edit_chain.txt",
                "old_string": "original",
                "new_string": "modified",
                "replace_all": false
            })),
        )
        .await;

    // Read
    let read_outcome = dispatcher
        .execute(
            "tc-14c",
            "Read",
            &tool_args(serde_json::json!({"file_path": "edit_chain.txt"})),
        )
        .await;

    assert!(!read_outcome.result.is_error);
    assert!(read_outcome.result.output.contains("modified text"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.8 Tool property validation across registry
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn registry_tools_have_valid_descriptors() {
    let registry = test_registry();
    let descriptors = registry.all_descriptors();
    assert!(!descriptors.is_empty(), "registry should have tools");

    for desc in &descriptors {
        assert!(!desc.name.is_empty(), "tool name must not be empty");
        assert!(
            desc.input_schema.get("type").is_some(),
            "{}: input_schema missing type",
            desc.name
        );
    }
}

#[test]
fn read_only_tools_marked_correctly() {
    let registry = test_registry();
    let read_only_tools = ["Read", "Glob", "Grep", "WebFetch", "WebSearch"];

    for name in &read_only_tools {
        let tool = registry
            .get_tool_arc(name)
            .unwrap_or_else(|| panic!("{name} not found"));
        let desc = tool.descriptor();
        assert!(desc.is_read_only, "{name} should be read-only");
    }
}

#[test]
fn dangerous_tools_not_read_only() {
    let registry = test_registry();
    let dangerous = ["Write", "Edit", "Bash"];

    for name in &dangerous {
        let tool = registry
            .get_tool_arc(name)
            .unwrap_or_else(|| panic!("{name} not found"));
        let desc = tool.descriptor();
        assert!(!desc.is_read_only, "{name} should not be read-only");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.9 Tool error handling at dispatcher level
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn unknown_tool_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    let outcome = dispatcher
        .execute("tc-15", "NonExistentTool", &serde_json::json!({}))
        .await;

    assert!(outcome.result.is_error);
    assert!(outcome.result.output.contains("not found"));
}

#[tokio::test]
async fn invalid_tool_input_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = test_dispatcher(tmp.path().to_path_buf());

    // Read without required file_path
    let outcome = dispatcher
        .execute("tc-16", "Read", &serde_json::json!({}))
        .await;

    assert!(outcome.result.is_error);
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.10 Executor caching for read-only tools
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn read_only_tool_result_cached() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("cache.txt"), "cached content").unwrap();

    let config = ExecutionConfig {
        cache_config: ToolCacheConfig {
            enabled: true,
            ..Default::default()
        },
        ..ExecutionConfig::default()
    };
    let registry = test_registry();
    let executor = Arc::new(ToolsExecutor::new(config));
    let dispatcher = RegistryToolDispatcher::new(
        registry,
        executor,
        ExecutionContext::Normal,
        tmp.path().to_path_buf(),
    );

    // First call
    let outcome1 = dispatcher
        .execute(
            "tc-17a",
            "Read",
            &tool_args(serde_json::json!({"file_path": "cache.txt"})),
        )
        .await;
    assert!(!outcome1.result.is_error);

    // Modify file behind the cache
    std::fs::write(tmp.path().join("cache.txt"), "modified content").unwrap();

    // Second call should return cached result (old content)
    let outcome2 = dispatcher
        .execute(
            "tc-17b",
            "Read",
            &tool_args(serde_json::json!({"file_path": "cache.txt"})),
        )
        .await;
    assert!(!outcome2.result.is_error);
    // Should still have original content due to caching
    assert!(outcome2.result.output.contains("cached content"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.11 Tool schema validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn all_registered_tools_have_input_output_schemas() {
    let registry = test_registry();
    for desc in registry.all_descriptors() {
        assert!(
            desc.input_schema.is_object() || desc.input_schema.is_object(),
            "{}: input_schema should be a JSON object",
            desc.name
        );
        assert!(
            desc.output_schema.is_object(),
            "{}: output_schema should be a JSON object",
            desc.name
        );
    }
}

#[test]
fn all_tool_names_are_unique_in_registry() {
    let registry = test_registry();
    let names: Vec<String> = registry.list_tools().into_iter().map(|(n, _)| n).collect();
    let unique_count = names
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert_eq!(
        unique_count,
        names.len(),
        "registry contains duplicate tool names"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.12 Sandbox policy integration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn sandbox_denies_write_outside_allowed_paths() {
    let tmp = tempfile::tempdir().unwrap();
    // Build dispatcher with sandbox ENABLED
    let registry = test_registry();
    let executor = Arc::new(ToolsExecutor::new(ExecutionConfig::default()));
    let mut dispatcher = RegistryToolDispatcher::new(
        registry,
        executor,
        ExecutionContext::Normal,
        tmp.path().to_path_buf(),
    );

    // Set a sandbox config that only allows writes within tmp/allowed
    let sandbox = coderex_core::sandbox::SandboxConfig {
        allowed_read_paths: vec![tmp.path().to_path_buf()],
        allowed_write_paths: vec![tmp.path().join("allowed")],
        allow_network: false,
        ..Default::default()
    };
    dispatcher.set_sandbox_override(Some(sandbox));

    // Writing to disallowed path should fail
    let outcome = dispatcher
        .execute(
            "tc-18",
            "Write",
            &tool_args(serde_json::json!({
                "file_path": tmp.path().join("disallowed").join("file.txt"),
                "content": "x"
            })),
        )
        .await;

    assert!(outcome.result.is_error);
    assert!(
        outcome.result.output.contains("permission denied")
            || outcome.result.output.contains("PermissionDenied")
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.13 Tool tier filtering
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn plan_mode_filters_to_read_only_tools() {
    let registry = test_registry();
    let names = registry.available_tool_names(ExecutionContext::PlanMode, true);

    // Write, Edit, Bash should not be available in plan mode
    assert!(!names.contains(&"Write".to_string()));
    assert!(!names.contains(&"Edit".to_string()));
    assert!(!names.contains(&"Bash".to_string()));

    // Read, Glob, Grep should be available
    assert!(names.contains(&"Read".to_string()));
    assert!(names.contains(&"Glob".to_string()));
    assert!(names.contains(&"Grep".to_string()));
}

#[test]
fn normal_mode_has_all_baseline_tools() {
    let registry = test_registry();
    let names = registry.available_tool_names(ExecutionContext::Normal, true);

    assert!(names.contains(&"Read".to_string()));
    assert!(names.contains(&"Write".to_string()));
    assert!(names.contains(&"Edit".to_string()));
    assert!(names.contains(&"Bash".to_string()));
    assert!(names.contains(&"Glob".to_string()));
    assert!(names.contains(&"Grep".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.14 Tool context allowlist filtering
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn context_allowlist_restricts_tools() {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);

    let mut allowlist = std::collections::HashSet::new();
    allowlist.insert("Read".to_string());
    allowlist.insert("Glob".to_string());
    registry.set_context_allowlist(ExecutionContext::HeadlessSdk, allowlist);

    let names = registry.available_tool_names(ExecutionContext::HeadlessSdk, true);
    assert!(names.contains(&"Read".to_string()));
    assert!(names.contains(&"Glob".to_string()));
    assert!(!names.contains(&"Write".to_string()));
    assert!(!names.contains(&"Bash".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.15 Registry deny rules
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn deny_rule_blocks_tool_availability() {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);
    registry.add_deny_rule(coderex_tools::ToolDenyRule::for_tools(vec!["Bash".into()]));

    let names = registry.available_tool_names(ExecutionContext::Normal, true);
    assert!(!names.contains(&"Bash".to_string()));
    assert!(names.contains(&"Read".to_string()));
}
