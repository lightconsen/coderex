//! Sandbox guarantees observable through the real binary: a destructive shell
//! command cannot touch a path outside the workspace, and the provider
//! credential never reaches the shell child.

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

fn run(cwd: &Path, base_url: &str, prompt: &str, api_key: &str) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args([
            "-p",
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
        ])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", api_key)
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
    child.wait_with_output().expect("wait")
}

#[cfg(unix)]
#[test]
fn a_destructive_shell_command_cannot_touch_a_path_outside_the_workspace() {
    let dir = tempfile::tempdir().expect("workspace");
    // Exactly the path the static provider's destructive command targets.
    let victim = PathBuf::from("/tmp/coderex-benchmark-should-not-run");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("sentinel"), "keep").unwrap();
    let (mut provider, base_url) = start_static_provider();

    let output = run(
        dir.path(),
        &base_url,
        "benchmark malicious shell",
        "mock-e2e-key",
    );

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
        "the destructive command reached a path outside the workspace"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Operation not permitted")
            || stdout.contains("Permission denied")
            || stdout.contains("not allowed"),
        "the command failed for a reason other than confinement:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&victim);
}

#[test]
fn the_shell_child_does_not_see_the_provider_credential() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();
    let key = "sk-coderex-e2e-secret";

    let output = run(dir.path(), &base_url, "print the provider key", key);

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(key),
        "the provider key reached the shell/tool result:\n{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn the_sandbox_denies_network_access_for_shell_commands() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();

    let output = run(
        dir.path(),
        &base_url,
        "try a network request",
        "mock-e2e-key",
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let tool_result = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .flat_map(|frame| {
            frame["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|block| block["type"] == "tool_result")
        .filter_map(|block| block["content"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!tool_result.is_empty(), "no tool result:\n{stdout}");
    assert!(
        !tool_result.contains("Example Domain"),
        "the sandbox let the network request through: {tool_result}"
    );
    assert!(
        tool_result.contains("Operation not permitted")
            || tool_result.contains("Could not resolve")
            || tool_result.contains("Network is unreachable")
            || tool_result.contains("Connection refused")
            || tool_result.contains("[Exit code: 6]")
            || tool_result.contains("[Exit code: 7]"),
        "unexpected network result: {tool_result}"
    );
}
