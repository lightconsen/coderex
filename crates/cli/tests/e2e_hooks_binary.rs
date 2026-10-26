//! Hook firing through the real binary.
//!
//! The hooks crate tests the registry and the TUI tests one blocking hook;
//! these prove what a user gets from `coderex -p`: a failed gating hook denies
//! the tool, a Stop hook runs after a completed turn, and a PostToolUse rewrite
//! reaches the model's tool result.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn start_static_provider() -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args([
            "--port",
            "0",
            "--responses",
            responses.to_str().expect("responses path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn static provider");

    let stderr = child.stderr.take().expect("server stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut announced = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !announced && let Some(address) = line.split("listening on ").nth(1) {
                let _ = tx.send(address.trim().to_string());
                announced = true;
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("static provider did not announce its address");
    (child, format!("http://{address}"))
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write hook script");
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn seed_config(settings: &serde_json::Value) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        serde_json::json!({
            "config_version": 4,
            "telemetry_enabled": false,
            "hooks": settings["hooks"].clone(),
        })
        .to_string(),
    )
    .expect("write settings");
    home
}

fn run(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    prompt: &str,
    args: &[&str],
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .arg("-p")
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODEREX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{prompt}\n").as_bytes())
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

fn hook_settings(event: &str, matcher: Option<&str>, command: &str) -> serde_json::Value {
    let mut hook = serde_json::json!({
        "event": event,
        "command": command,
        "timeout_ms": 5000,
        "sandbox": {"enabled": false}
    });
    if let Some(matcher) = matcher {
        hook["if"] = serde_json::Value::String(matcher.to_string());
    }
    serde_json::json!({"hooks": [hook]})
}

#[test]
fn a_failing_pretooluse_hook_denies_the_write() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let deny = script(dir.path(), "deny.sh", "exit 1");
    let home = seed_config(&hook_settings(
        "PreToolUse",
        Some("Write(*)"),
        deny.to_str().unwrap(),
    ));
    let (mut provider, base_url) = start_static_provider();

    let output = run(
        dir.path(),
        home.path(),
        &base_url,
        "add a recursive fibonacci function to src/lib.rs with unit tests",
        &["--dangerously-skip-permissions"],
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "a hook that failed on a gating event let the write through:\n{content}"
    );
}

#[test]
fn a_stop_hook_runs_after_a_completed_turn() {
    let dir = tempfile::tempdir().expect("workspace");
    let marker = dir.path().join("stop-hit");
    let stop = script(
        dir.path(),
        "stop.sh",
        &format!("printf hit >> {}", marker.display()),
    );
    let home = seed_config(&hook_settings("Stop", None, stop.to_str().unwrap()));
    let (mut provider, base_url) = start_static_provider();

    let output = run(
        dir.path(),
        home.path(),
        &base_url,
        "hello",
        &["--dangerously-skip-permissions"],
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker).expect("the Stop hook ran"),
        "hit"
    );
}

#[test]
fn a_posttooluse_rewrite_reaches_the_tool_result() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let fired = dir.path().join("hook-fired");
    let rewrite = script(
        dir.path(),
        "rewrite.sh",
        &format!(
            "printf hit >> {}\nprintf '%s\\n' '{{\"updatedToolOutput\":\"REDACTED-BY-HOOK\"}}'",
            fired.display()
        ),
    );
    let home = seed_config(&hook_settings(
        "PostToolUse",
        Some("Bash(*)"),
        rewrite.to_str().unwrap(),
    ));
    let (mut provider, base_url) = start_static_provider();

    let output = run(
        dir.path(),
        home.path(),
        &base_url,
        "run project tests",
        &[
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
        ],
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        fired.exists(),
        "the PostToolUse hook never fired:\n{stdout}"
    );
    assert!(
        stdout.contains("REDACTED-BY-HOOK"),
        "the PostToolUse rewrite never reached the transcript:\n{stdout}"
    );
}
