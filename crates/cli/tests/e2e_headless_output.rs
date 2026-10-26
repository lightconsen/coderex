//! Headless output fidelity through the real binary.
//!
//! `coderex -p` is piped and parsed, so stdout must be the model's answer and
//! nothing else: no session-title banner, and no content read from a file the
//! prompt merely names.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const GENERIC_ANSWER: &str = "Of course! I'd be happy to help. Please share the details of the task you're working on, and I'll do my best to assist you.";
const HELLO_ANSWER: &str = "Hello! Yes, I'd be happy to help you with coding.";

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

fn run_headless(cwd: &Path, base_url: &str, prompt: &str) -> String {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--dangerously-skip-permissions"])
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
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_prompt_that_names_a_file_does_not_leak_its_content() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("notes.txt"), "SECRET-FILE-CONTENT\n").unwrap();
    let (mut provider, base_url) = start_static_provider();

    let stdout = run_headless(dir.path(), &base_url, "summarize notes.txt");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        !stdout.contains("SECRET-FILE-CONTENT"),
        "the prompt-named file leaked into stdout:\n{stdout}"
    );
    assert_eq!(stdout.trim(), GENERIC_ANSWER);
}

#[test]
fn stdout_is_the_answer_without_a_title_banner() {
    let dir = tempfile::tempdir().expect("workspace");
    let (mut provider, base_url) = start_static_provider();

    let stdout = run_headless(dir.path(), &base_url, "hello");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        !stdout.contains("Session title"),
        "the headless answer is preceded by internal state:\n{stdout}"
    );
    assert!(
        stdout.trim_start().starts_with(HELLO_ANSWER),
        "unexpected answer:\n{stdout}"
    );
}
