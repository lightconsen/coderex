//! Integration tests for the per-session file checkpoint store used by
//! `/rewind`: content captured before an edit comes back, and files an undone
//! call created disappear.

use std::collections::HashSet;

use coderex_core::checkpoints::CheckpointStore;

fn ids(values: &[&str]) -> HashSet<String> {
    values
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
}

#[test]
fn restoring_the_earliest_snapshot_undoes_every_later_edit() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path()).unwrap();
    let file = root.path().join("lib.rs");
    std::fs::write(&file, "fn original() {}\n").unwrap();

    store.snapshot("call_1", &file);
    std::fs::write(&file, "fn first_edit() {}\n").unwrap();
    store.snapshot("call_2", &file);
    std::fs::write(&file, "fn second_edit() {}\n").unwrap();

    let report = store.restore(&ids(&["call_1", "call_2"]));

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn original() {}\n"
    );
    assert_eq!(report.restored, vec![file], "one path, once");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(report.removed.is_empty(), "{report:?}");
}

#[test]
fn restoring_only_a_later_call_keeps_the_first_edit() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path()).unwrap();
    let file = root.path().join("lib.rs");
    std::fs::write(&file, "fn original() {}\n").unwrap();

    store.snapshot("call_1", &file);
    std::fs::write(&file, "fn first_edit() {}\n").unwrap();
    store.snapshot("call_2", &file);
    std::fs::write(&file, "fn second_edit() {}\n").unwrap();

    let report = store.restore(&ids(&["call_2"]));

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "fn first_edit() {}\n"
    );
    assert_eq!(report.restored, vec![file]);
}

#[test]
fn a_file_created_by_an_undone_call_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path()).unwrap();
    let created = root.path().join("new_module.rs");

    store.snapshot("call_create", &created);
    std::fs::write(&created, "pub fn new() {}\n").unwrap();

    let report = store.restore(&ids(&["call_create"]));

    assert!(!created.exists(), "the created file is gone");
    assert_eq!(report.removed, vec![created]);
    assert!(report.restored.is_empty());
}

#[test]
fn calls_outside_the_rewind_set_are_untouched() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path()).unwrap();
    let kept = root.path().join("kept.rs");
    let undone = root.path().join("undone.rs");
    std::fs::write(&kept, "kept\n").unwrap();
    std::fs::write(&undone, "before\n").unwrap();

    store.snapshot("call_keep", &kept);
    store.snapshot("call_undo", &undone);
    std::fs::write(&kept, "kept edited\n").unwrap();
    std::fs::write(&undone, "undone edited\n").unwrap();

    store.restore(&ids(&["call_undo"]));

    assert_eq!(std::fs::read_to_string(&kept).unwrap(), "kept edited\n");
    assert_eq!(std::fs::read_to_string(&undone).unwrap(), "before\n");
}

#[cfg(unix)]
#[test]
fn an_unwritable_target_is_reported_rather_than_lost() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path()).unwrap();
    let dir = root.path().join("locked");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("lib.rs");
    std::fs::write(&file, "before\n").unwrap();

    store.snapshot("call_1", &file);
    std::fs::write(&file, "after\n").unwrap();
    let mut permissions = std::fs::metadata(&file).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&file, permissions).unwrap();

    let report = store.restore(&ids(&["call_1"]));

    // Restore the permissions so the tempdir can be cleaned up.
    let mut permissions = std::fs::metadata(&file).unwrap().permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&file, permissions).unwrap();

    assert_eq!(report.failed, vec![file], "{report:?}");
    assert!(report.restored.is_empty(), "{report:?}");
}

#[test]
fn dropping_the_store_removes_its_blob_directory() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("lib.rs");
    std::fs::write(&file, "before\n").unwrap();

    let (blob_dir, existed) = {
        let store = CheckpointStore::new(root.path()).unwrap();
        store.snapshot("call_1", &file);
        let blob_dir = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("session-")
            })
            .expect("session directory");
        let existed = blob_dir.exists();
        (blob_dir, existed)
    };
    assert!(existed, "the store made a session directory");

    // Dropping the store is the end of the session.
    assert!(!blob_dir.exists(), "the session's blobs were removed");
}
