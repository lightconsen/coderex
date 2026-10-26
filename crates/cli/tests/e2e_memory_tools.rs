//! Memory tools through the real binary: a write persists in the project
//! store, and a later session reads the value back.

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

fn run(cwd: &Path, home: &Path, base_url: &str, prompt: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args([
            "-p",
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
        ])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("HOME", home)
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
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_project_memory_written_in_one_session_is_read_in_the_next() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let (mut provider, base_url) = start_static_provider();

    let written = run(workspace.path(), home.path(), &base_url, "write a memory");
    assert!(
        written.contains("memo-value") || written.contains("e2e-memory"),
        "the write produced no tool result:\n{written}"
    );

    let read = run(workspace.path(), home.path(), &base_url, "read a memory");
    let tool_result = read
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

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        tool_result.contains("memo-value"),
        "the second session did not read the memory: {tool_result:?}\n{read}"
    );
}
