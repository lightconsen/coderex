//! A tool result over the display cap is truncated through the real binary:
//! the transcript carries the marker and not the omitted tail.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
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

#[test]
fn a_large_read_is_truncated_with_a_marker() {
    let dir = tempfile::tempdir().expect("workspace");
    let content = format!("HEAD\n{}\nTAIL_SHOULD_NOT_APPEAR\n", "x".repeat(30_000));
    std::fs::write(dir.path().join("large.txt"), content).expect("large file");
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let (mut provider, base_url) = start_static_provider();

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args([
            "-p",
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
        ])
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
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
        .write_all(b"read the large file\n")
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("chars omitted"),
        "the large read was not truncated:\n{stdout}"
    );
    assert!(
        !stdout.contains("TAIL_SHOULD_NOT_APPEAR"),
        "the truncated tail still reached the transcript"
    );
}
