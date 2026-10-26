//! `--permissions auto` through the real binary: a safe shell command runs
//! without asking the host, and a destructive one does not.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

fn spawn(cwd: &Path, base_url: &str, prompt: &str, args: &[&str]) -> (Child, tempfile::TempDir) {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .arg("-p")
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
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
    (child, config_home)
}

#[test]
fn a_safe_shell_command_runs_without_a_control_request() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();
    let (mut child, _home) = spawn(
        dir.path(),
        &base_url,
        "run a safe echo",
        &["--permissions", "auto", "--output-format", "stream-json"],
    );

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut tool_result = String::new();
    loop {
        let line = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the turn produced no result frame");
        let frame: serde_json::Value =
            serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
        match frame["type"].as_str().unwrap_or_default() {
            "control_request" => panic!(
                "auto mode asked the host to approve a safe command: {}",
                frame["request"]
            ),
            "user" => {
                if let Some(content) = frame["message"]["content"].as_array() {
                    for block in content {
                        if block["type"] == "tool_result" {
                            tool_result = block["content"].as_str().unwrap_or_default().to_string();
                        }
                    }
                }
            }
            "result" => break,
            _ => {}
        }
    }

    let status = child.wait().expect("wait");
    let _ = provider.kill();
    let _ = provider.wait();
    assert!(status.success());
    assert!(
        tool_result.contains("auto-safe"),
        "the safe command did not run: {tool_result:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_destructive_shell_command_is_blocked_in_auto_mode() {
    let dir = tempfile::tempdir().expect("workspace");
    let victim = std::path::PathBuf::from("/tmp/coderex-benchmark-should-not-run");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("sentinel"), "keep").unwrap();
    let (mut provider, base_url) = start_static_provider();

    let (child, _home) = spawn(
        dir.path(),
        &base_url,
        "benchmark malicious shell",
        &["--permissions", "auto"],
    );
    let output = child.wait_with_output().expect("wait");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(victim.join("sentinel")).ok(),
        Some("keep".to_string()),
        "auto mode let a destructive command run"
    );
    let _ = std::fs::remove_dir_all(&victim);
}
