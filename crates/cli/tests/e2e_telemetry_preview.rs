//! `coderex telemetry preview-crash` through the real binary: the command
//! prints the wire payload for a pending crash, locally, without uploading.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run(config_home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(args)
        .env("CLAUDE_CONFIG_DIR", config_home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run coderex")
}

fn write_pending(config_home: &Path, name: &str) -> PathBuf {
    let pending = config_home.join("crashes").join("pending");
    std::fs::create_dir_all(&pending).expect("pending dir");
    let path = pending.join(name);
    std::fs::write(
        &path,
        serde_json::json!({
            "panic_info": "called `Option::unwrap()` on a `None` value",
            "timestamp": "2026-10-26T00:00:00Z"
        })
        .to_string(),
    )
    .expect("write pending crash");
    path
}

#[test]
fn the_newest_pending_crash_is_previewed_as_structured_json() {
    let config_home = tempfile::tempdir().expect("config home");
    write_pending(config_home.path(), "crash.json");

    let output = run(config_home.path(), &["telemetry", "preview-crash"]);

    assert!(
        output.status.success(),
        "preview-crash failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).expect("JSON payload");
    assert!(payload.is_object(), "{payload}");
    assert!(
        payload.get("panic_info").is_none(),
        "the default payload is the structured form, not free-form panic text: {payload}"
    );
    assert!(
        payload.get("panic_message_sha256").is_some(),
        "the payload identifies the panic without carrying it: {payload}"
    );
}

#[test]
fn an_explicit_pending_file_is_previewed() {
    let config_home = tempfile::tempdir().expect("config home");
    let path = write_pending(config_home.path(), "explicit.json");

    let output = run(
        config_home.path(),
        &["telemetry", "preview-crash", path.to_str().unwrap()],
    );

    assert!(
        output.status.success(),
        "preview-crash failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("JSON");
    assert!(payload.is_object(), "{payload}");
}

#[test]
fn no_pending_crash_is_a_clear_error() {
    let config_home = tempfile::tempdir().expect("config home");

    let output = run(config_home.path(), &["telemetry", "preview-crash"]);

    assert!(!output.status.success(), "expected a non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("no pending crash"),
        "unexpected error: {stderr}"
    );
}
