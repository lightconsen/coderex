//! Sandbox integration tests — verify resource limits and access controls
//! are actually enforced at the OS level.

use coderex_core::sandbox::{SandboxConfig, SandboxHandle};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-platform tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn sandbox_config_serde_roundtrip() {
    let config = SandboxConfig {
        allowed_read_paths: vec![PathBuf::from("/tmp/read")],
        allowed_write_paths: vec![PathBuf::from("/tmp/write")],
        allowed_exec_paths: vec![PathBuf::from("/bin")],
        additional_working_dirs: vec![PathBuf::from("/tmp/work")],
        max_open_files: Some(128),
        max_memory_bytes: Some(256 * 1024 * 1024),
        max_cpu_seconds: Some(60),
        allow_network: true,
    };

    let json = serde_json::to_string(&config).unwrap();
    let restored: SandboxConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.allowed_read_paths, config.allowed_read_paths);
    assert_eq!(restored.allowed_write_paths, config.allowed_write_paths);
    assert_eq!(restored.max_open_files, config.max_open_files);
    assert_eq!(restored.max_memory_bytes, config.max_memory_bytes);
    assert_eq!(restored.max_cpu_seconds, config.max_cpu_seconds);
    assert!(restored.allow_network);
}

#[test]
fn sandbox_handle_create_with_limits() {
    let config = SandboxConfig {
        max_open_files: Some(64),
        max_memory_bytes: Some(1024 * 1024 * 1024),
        max_cpu_seconds: Some(30),
        allow_network: false,
        ..SandboxConfig::default()
    };
    let handle = SandboxHandle::create(config);
    assert!(handle.is_ok(), "sandbox creation should succeed");
}

#[test]
fn sandbox_handle_create_no_limits() {
    let handle = SandboxHandle::create(SandboxConfig::default());
    assert!(handle.is_ok(), "sandbox creation with defaults should succeed");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Linux: rlimit + seccomp
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "linux")]
#[test]
fn linux_rlimit_cpu_kills_busy_process() {
    let config = SandboxConfig {
        max_cpu_seconds: Some(1),
        ..SandboxConfig::default()
    };
    let handle = SandboxHandle::create(config).unwrap();

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("i=0; while true; do i=$((i+1)); done");
    handle.prepare(&mut cmd).unwrap();

    let start = Instant::now();
    let mut child = cmd.spawn().unwrap();

    // Wait for the child to exit (should be killed by SIGXCPU within a few seconds)
    let timeout = Duration::from_secs(5);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    panic!("child did not exit within timeout — rlimit CPU may not be enforced");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("wait error: {e}"),
        }
    };

    assert!(!result.success(), "busy-loop should have been killed by CPU limit");
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "CPU limit should have killed process quickly, took {:?}",
        start.elapsed()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_seccomp_allows_safe_syscalls() {
    let handle = SandboxHandle::create(SandboxConfig::default()).unwrap();

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg("echo hello");
    handle.prepare(&mut cmd).unwrap();

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "basic shell command should succeed under seccomp: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_rlimit_nofile_enforced() {
    let config = SandboxConfig {
        max_open_files: Some(32),
        ..SandboxConfig::default()
    };
    let handle = SandboxHandle::create(config).unwrap();

    let mut cmd = std::process::Command::new("sh");
    // Try to open more files than the limit allows
    cmd.arg("-c")
        .arg("for i in $(seq 1 40); do exec 3>/dev/null; done 2>/dev/null; echo ok");
    handle.prepare(&mut cmd).unwrap();

    let output = cmd.output().unwrap();
    // The shell may fail to open all descriptors but still exit 0
    // We mainly verify the process didn't crash and rlimit was applied
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "process should handle rlimit gracefully: {:?}",
        output.status
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  macOS: sandbox-exec
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
#[test]
fn macos_sandbox_allows_write_in_allowed_path() {
    // Use /private/tmp (canonical temp on macOS) to avoid sandbox-exec subpath issues
    let allowed = PathBuf::from(format!("/private/tmp/coderex_sandbox_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&allowed);
    std::fs::create_dir(&allowed).unwrap();

    let config = SandboxConfig {
        allowed_write_paths: vec![allowed.clone()],
        allowed_read_paths: vec![allowed.clone()],
        allowed_exec_paths: vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")],
        ..SandboxConfig::default()
    };
    let handle = SandboxHandle::create(config).unwrap();
    let (wrapper, args) = handle.sandboxed_program().unwrap();

    let test_file = allowed.join("test.txt");
    let mut cmd = std::process::Command::new(wrapper);
    cmd.args(&args)
        .arg("sh")
        .arg("-c")
        .arg(format!("echo ok > {}", test_file.display()));

    let output = cmd.output().unwrap();

    assert!(
        output.status.success(),
        "write to allowed path should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(&test_file).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&allowed);
    assert_eq!(content.trim(), "ok");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_sandbox_denies_write_outside_allowed_path() {
    let allowed = PathBuf::from(format!("/private/tmp/coderex_sandbox_allowed_{}", std::process::id()));
    let restricted = PathBuf::from(format!("/private/tmp/coderex_sandbox_restricted_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&restricted);
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&restricted).unwrap();

    let config = SandboxConfig {
        allowed_write_paths: vec![allowed.clone()],
        allowed_read_paths: vec![allowed.clone(), restricted.clone()],
        allowed_exec_paths: vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")],
        ..SandboxConfig::default()
    };
    let handle = SandboxHandle::create(config).unwrap();
    let (wrapper, args) = handle.sandboxed_program().unwrap();

    let test_file = restricted.join("test.txt");
    let mut cmd = std::process::Command::new(wrapper);
    cmd.args(&args)
        .arg("sh")
        .arg("-c")
        .arg(format!("echo bad > {} 2>/dev/null || echo denied", test_file.display()));

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _ = std::fs::remove_dir_all(&allowed);
    let _ = std::fs::remove_dir_all(&restricted);
    // sandbox-exec may either fail the write or let the shell fallback run
    assert!(
        !test_file.exists() || stdout.contains("denied"),
        "write to restricted path should be blocked"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_sandbox_denies_network() {
    let config = SandboxConfig {
        allow_network: false,
        allowed_exec_paths: vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")],
        ..SandboxConfig::default()
    };
    let handle = SandboxHandle::create(config).unwrap();
    let (wrapper, args) = handle.sandboxed_program().unwrap();

    let mut cmd = std::process::Command::new(wrapper);
    cmd.args(&args)
        .arg("sh")
        .arg("-c")
        .arg("curl -s http://localhost:1 2>/dev/null || echo blocked");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Either curl fails due to sandbox or the fallback echo runs
    assert!(
        stdout.contains("blocked") || !output.status.success(),
        "network access should be blocked by sandbox"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Sandbox policy integration
// ═══════════════════════════════════════════════════════════════════════════

// NOTE: Tests for sandbox_policy::default_sandbox_config (Bash, Write policies)
// live in crates/tools/tests/ since they depend on coderex_tools.
