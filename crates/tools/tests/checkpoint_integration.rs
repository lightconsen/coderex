//! Integration tests for the file-checkpoint producers.
//!
//! `/rewind` restores files only if the mutating tools captured them before
//! writing. These drive the real `Write` and `Edit` tools through the
//! dispatcher with a session `CheckpointStore`, then restore and look at the
//! file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use coderex_core::checkpoints::CheckpointStore;
use coderex_core::tool_executor::ToolExecutor;
use coderex_tools::{
    ExecutionConfig, ExecutionContext, RegistryToolDispatcher, ToolExecutor as ToolsExecutor,
    ToolRegistry, register_builtin_tools,
};

fn dispatcher(cwd: &Path, store: Option<Arc<CheckpointStore>>) -> RegistryToolDispatcher {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);
    let mut config = ExecutionConfig::default();
    config.sandbox_enabled = false;
    let executor = Arc::new(ToolsExecutor::new(config));
    let mut dispatcher = RegistryToolDispatcher::new(
        Arc::new(registry),
        executor,
        ExecutionContext::Normal,
        cwd.to_path_buf(),
    );
    dispatcher.set_sandbox_enabled(false);
    if let Some(store) = store {
        dispatcher.set_checkpoint_store(store);
    }
    dispatcher
}

fn ids(values: &[&str]) -> HashSet<String> {
    values
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
}

fn store(root: &Path) -> Arc<CheckpointStore> {
    Arc::new(CheckpointStore::new(root).expect("checkpoint store"))
}

#[tokio::test]
async fn write_over_an_existing_file_is_reversible() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("lib.rs");
    std::fs::write(&file, "fn original() {}\n").unwrap();
    let checkpoints = store(tmp.path());
    let dispatcher = dispatcher(tmp.path(), Some(Arc::clone(&checkpoints)));

    let outcome = dispatcher
        .execute(
            "call_write",
            "Write",
            &serde_json::json!({"file_path": "lib.rs", "content": "fn replaced() {}\n"}),
        )
        .await;
    assert!(!outcome.result.is_error, "{:?}", outcome.result);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn replaced() {}\n"
    );

    let report = checkpoints.restore(&ids(&["call_write"]));
    assert_eq!(
        report.restored,
        vec![std::fs::canonicalize(&file).unwrap()],
        "{report:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn original() {}\n"
    );
}

#[tokio::test]
async fn a_file_created_by_write_is_removed_on_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let checkpoints = store(tmp.path());
    let dispatcher = dispatcher(tmp.path(), Some(Arc::clone(&checkpoints)));

    let outcome = dispatcher
        .execute(
            "call_new",
            "Write",
            &serde_json::json!({"file_path": "new.rs", "content": "pub fn new() {}\n"}),
        )
        .await;
    assert!(!outcome.result.is_error, "{:?}", outcome.result);
    let file = tmp.path().join("new.rs");
    assert!(file.exists());
    let canonical = std::fs::canonicalize(&file).unwrap();

    let report = checkpoints.restore(&ids(&["call_new"]));
    assert_eq!(report.removed, vec![canonical], "{report:?}");
    assert!(!file.exists());
}

#[tokio::test]
async fn edit_snapshots_before_it_changes_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("main.rs");
    std::fs::write(&file, "fn main() {\n    old();\n}\n").unwrap();
    let checkpoints = store(tmp.path());
    let dispatcher = dispatcher(tmp.path(), Some(Arc::clone(&checkpoints)));

    let outcome = dispatcher
        .execute(
            "call_edit",
            "Edit",
            &serde_json::json!({
                "file_path": "main.rs",
                "old_string": "old();",
                "new_string": "new();",
            }),
        )
        .await;
    assert!(!outcome.result.is_error, "{:?}", outcome.result);
    assert!(std::fs::read_to_string(&file).unwrap().contains("new();"));

    let report = checkpoints.restore(&ids(&["call_edit"]));
    assert_eq!(
        report.restored,
        vec![std::fs::canonicalize(&file).unwrap()],
        "{report:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn main() {\n    old();\n}\n"
    );
}

#[tokio::test]
async fn two_writes_restore_to_the_content_before_the_first() {
    let tmp = tempfile::tempdir().unwrap();
    let file: PathBuf = tmp.path().join("lib.rs");
    std::fs::write(&file, "one\n").unwrap();
    let checkpoints = store(tmp.path());
    let dispatcher = dispatcher(tmp.path(), Some(Arc::clone(&checkpoints)));

    for (call, content) in [("call_a", "two\n"), ("call_b", "three\n")] {
        let outcome = dispatcher
            .execute(
                call,
                "Write",
                &serde_json::json!({"file_path": "lib.rs", "content": content}),
            )
            .await;
        assert!(!outcome.result.is_error, "{:?}", outcome.result);
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "three\n");

    checkpoints.restore(&ids(&["call_a", "call_b"]));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
}

#[tokio::test]
async fn a_tool_without_a_store_still_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = dispatcher(tmp.path(), None);

    let outcome = dispatcher
        .execute(
            "call_plain",
            "Write",
            &serde_json::json!({"file_path": "plain.rs", "content": "ok\n"}),
        )
        .await;

    assert!(!outcome.result.is_error, "{:?}", outcome.result);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("plain.rs")).unwrap(),
        "ok\n"
    );
}
