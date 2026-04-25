//! Sandbox policy integration tests — verify per-tool sandbox configs.

#[test]
fn sandbox_policy_bash_has_restrictive_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Bash", tmp.path());
    assert!(config.is_some(), "Bash should have a sandbox policy");
    let config = config.unwrap();
    assert!(!config.allow_network, "Bash should not have network access");
    assert!(
        config.max_cpu_seconds.is_some(),
        "Bash should have CPU limit"
    );
    assert!(
        config.max_memory_bytes.is_some(),
        "Bash should have memory limit"
    );
    assert!(config.max_open_files.is_some(), "Bash should have fd limit");
}

#[test]
fn sandbox_policy_write_has_restrictive_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Write", tmp.path());
    assert!(config.is_some(), "Write should have a sandbox policy");
    let config = config.unwrap();
    assert!(
        !config.allow_network,
        "Write should not have network access"
    );
    assert!(
        config.max_cpu_seconds.is_some(),
        "Write should have CPU limit"
    );
    assert!(
        config.max_memory_bytes.is_some(),
        "Write should have memory limit"
    );
}

#[test]
fn sandbox_policy_edit_has_restrictive_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Edit", tmp.path());
    assert!(config.is_some(), "Edit should have a sandbox policy");
    let config = config.unwrap();
    assert!(!config.allow_network, "Edit should not have network access");
}

#[test]
fn sandbox_policy_bash_limits_are_reasonable() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Bash", tmp.path()).unwrap();

    // Bash should have the most permissive limits among sandboxed tools
    assert_eq!(config.max_open_files, Some(256));
    assert_eq!(config.max_memory_bytes, Some(512 * 1024 * 1024)); // 512 MB
    assert_eq!(config.max_cpu_seconds, Some(300)); // 5 minutes
}

#[test]
fn sandbox_policy_write_limits_are_more_restrictive() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Write", tmp.path()).unwrap();

    assert_eq!(config.max_open_files, Some(64));
    assert_eq!(config.max_memory_bytes, Some(64 * 1024 * 1024)); // 64 MB
    assert_eq!(config.max_cpu_seconds, Some(60)); // 1 minute
}

#[test]
fn sandbox_policy_paths_include_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Bash", tmp.path()).unwrap();

    let canonical = std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
    assert!(
        config.allowed_read_paths.iter().any(|p| p == &canonical),
        "cwd should be in allowed_read_paths"
    );
    assert!(
        config.allowed_write_paths.iter().any(|p| p == &canonical),
        "cwd should be in allowed_write_paths"
    );
}

#[test]
fn sandbox_policy_bash_has_system_exec_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Bash", tmp.path()).unwrap();

    assert!(
        !config.allowed_exec_paths.is_empty(),
        "Bash needs system exec paths"
    );
}

#[test]
fn sandbox_policy_write_has_no_exec_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("Write", tmp.path()).unwrap();

    assert!(
        config.allowed_exec_paths.is_empty(),
        "Write should not execute anything"
    );
}

#[test]
fn sandbox_policy_unknown_tool_gets_default() {
    let tmp = tempfile::tempdir().unwrap();
    let config = coderex_tools::default_sandbox_config("SomeUnknownTool", tmp.path());
    assert!(
        config.is_some(),
        "unknown tools should get a default policy"
    );
}

#[test]
fn sandbox_policy_case_insensitive_tool_names() {
    let tmp = tempfile::tempdir().unwrap();

    let bash_lower = coderex_tools::default_sandbox_config("bash", tmp.path());
    let bash_upper = coderex_tools::default_sandbox_config("Bash", tmp.path());
    assert!(bash_lower.is_some());
    assert!(bash_upper.is_some());

    let write_lower = coderex_tools::default_sandbox_config("write", tmp.path());
    let write_upper = coderex_tools::default_sandbox_config("Write", tmp.path());
    assert!(write_lower.is_some());
    assert!(write_upper.is_some());
}
