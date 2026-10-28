//! What a headless run says when a tool needs approval and nothing can answer.
//!
//! `coderex -p` has no confirmation channel unless the host asked for one. A
//! tool that needs approval is then not denied by a person — nobody was asked —
//! and the model is told that, so it can try a different approach instead of
//! believing a user refused it. The wording existed in the engine and had only
//! unit tests; this proves it through the binary, and that the same run with a
//! control channel does say "denied by user" when the person declines.

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

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    dir
}

/// A headless turn with no control channel at all: plain `-p`, piped stdin.
fn run_without_control(dir: &Path, base_url: &str) -> String {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .arg("-p")
        .current_dir(dir)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
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
        .write_all(b"write a file inside the worktree\n")
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The model sees the tool result; `-p` prints only the answer, so the
    // refusal is read from the persisted transcript.
    let transcripts = dir.join(".coderex/transcripts");
    let mut text = String::new();
    for entry in std::fs::read_dir(&transcripts).expect("transcripts were not written") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
        }
    }
    text
}

/// A `Write` that nobody can approve is reported as a missing channel, and the
/// file is untouched.
#[test]
fn a_tool_with_no_confirmation_channel_is_not_a_user_denial() {
    let dir = workspace();
    let (mut provider, base_url) = start_static_provider();

    let text = run_without_control(dir.path(), &base_url);

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        !std::fs::read_to_string(dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "an unapproved write landed:\n{text}"
    );
    assert!(
        text.contains("no confirmation channel"),
        "the run did not explain that nothing could answer:\n{text}"
    );
    assert!(
        !text.contains("denied by user"),
        "an unasked tool was reported as a user denial:\n{text}"
    );
}
