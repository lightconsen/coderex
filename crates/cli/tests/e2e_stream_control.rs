//! stream-json tool approvals through the real binary: an `Ask` becomes a
//! `control_request` frame, and the host's `control_response` on stdin decides
//! whether the tool runs.

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

struct Outcome {
    saw_request: bool,
    control_requests: usize,
    tool: String,
    tool_result: String,
    tool_results: Vec<String>,
    frames: Vec<serde_json::Value>,
}

fn run_with_control(cwd: &Path, base_url: &str, prompt: &str, behavior: &str) -> Outcome {
    run_with_control_responses(cwd, base_url, prompt, &[behavior])
}

fn run_with_control_responses(
    cwd: &Path,
    base_url: &str,
    prompt: &str,
    behaviors: &[&str],
) -> Outcome {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--output-format", "stream-json"])
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

    let mut stdin = Some(child.stdin.take().expect("stdin"));
    stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("{prompt}\n").as_bytes())
        .expect("write prompt");
    stdin
        .as_mut()
        .expect("stdin")
        .flush()
        .expect("flush prompt");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut outcome = Outcome {
        saw_request: false,
        control_requests: 0,
        tool: String::new(),
        tool_result: String::new(),
        tool_results: Vec::new(),
        frames: Vec::new(),
    };
    loop {
        let line = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the turn produced no result frame");
        let frame: serde_json::Value =
            serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
        match frame["type"].as_str().unwrap_or_default() {
            "control_request" => {
                outcome.saw_request = true;
                outcome.control_requests += 1;
                outcome.tool = frame["request"]["tool_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let request_id = frame["request_id"].as_str().expect("request id");
                for behavior in behaviors {
                    let response = serde_json::json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": {"behavior": behavior, "message": "test decision"}
                        }
                    });
                    let pipe = stdin.as_mut().expect("stdin");
                    pipe.write_all(format!("{response}\n").as_bytes())
                        .expect("write control response");
                    pipe.flush().expect("flush control response");
                }
                // The host is done answering: closing stdin is the signal that
                // lets the headless session exit (and the control reader
                // finish).
                drop(stdin.take());
            }
            "user" => {
                if let Some(content) = frame["message"]["content"].as_array() {
                    for block in content {
                        if block["type"] == "tool_result" {
                            let text = block["content"].as_str().unwrap_or_default().to_string();
                            outcome.tool_result = text.clone();
                            outcome.tool_results.push(text);
                        }
                    }
                }
            }
            "result" => {
                outcome.frames.push(frame);
                break;
            }
            _ => {}
        }
    }

    let status = child.wait().expect("wait");
    assert!(status.success(), "coderex failed");
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

#[test]
fn an_approval_response_lets_the_tool_run() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let (mut provider, base_url) = start_static_provider();

    let outcome = run_with_control(dir.path(), &base_url, "run project tests", "allow");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(outcome.saw_request, "no control_request was emitted");
    assert_eq!(outcome.tool, "Bash");
    assert!(
        outcome.tool_result.contains("test result: ok"),
        "the approved command did not run: {:?}",
        outcome.tool_result
    );
}

#[test]
fn a_denial_response_keeps_the_tool_from_running() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let (mut provider, base_url) = start_static_provider();

    let outcome = run_with_control(
        dir.path(),
        &base_url,
        "add a recursive fibonacci function to src/lib.rs with unit tests",
        "deny",
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(outcome.saw_request, "no control_request was emitted");
    assert_eq!(outcome.tool, "Write");
    assert!(
        !std::fs::read_to_string(dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "a denied Write still changed the file"
    );
    assert!(
        !outcome.tool_result.is_empty(),
        "the denial produced no tool result"
    );
}

/// A command that runs as another user is an elevation question, not an
/// ordinary approval. The same control channel answers it, and the decision
/// decides whether the command is retried with privilege.
#[test]
fn an_elevation_request_can_be_approved_or_denied() {
    let approved = {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let (mut provider, base_url) = start_static_provider();
        let outcome = run_with_control(dir.path(), &base_url, "run a command with sudo", "allow");
        let _ = provider.kill();
        let _ = provider.wait();
        outcome
    };
    assert!(approved.saw_request, "no control_request for the elevation");
    assert_eq!(approved.tool, "Bash");
    assert!(
        !approved.tool_result.contains("elevation denied"),
        "an approved elevation was denied: {:?}",
        approved.tool_result
    );
    // The decision has to reach the call: the command ran, which it could not
    // have while the elevation was still pending. The marker is what proves
    // the retry happened rather than the tool being reported as approved and
    // then skipped.
    assert!(
        approved.tool_result.contains("elevation-marker"),
        "the approved command was not retried: {:?}",
        approved.tool_result
    );

    let denied = {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let (mut provider, base_url) = start_static_provider();
        let outcome = run_with_control(dir.path(), &base_url, "run a command with sudo", "deny");
        let _ = provider.kill();
        let _ = provider.wait();
        outcome
    };
    assert!(denied.saw_request, "no control_request for the elevation");
    assert!(
        denied.tool_result.contains("elevation denied"),
        "a denied elevation reported something else: {:?}",
        denied.tool_result
    );
    assert!(
        !denied.tool_result.contains("elevation-marker"),
        "a denied elevation still ran the command: {:?}",
        denied.tool_result
    );
    // A refusal by a person is the one case that may say so.
    assert!(
        denied.tool_result.contains("denied by user")
            || denied.tool_result.contains("elevation denied"),
        "the refusal did not name the decision: {:?}",
        denied.tool_result
    );
}

/// Two answers for the same pending approval must still produce one tool
/// result: the second response has nothing left to resolve.
#[test]
fn a_duplicate_control_response_produces_one_result() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let (mut provider, base_url) = start_static_provider();

    let outcome = run_with_control_responses(
        dir.path(),
        &base_url,
        "add a recursive fibonacci function to src/lib.rs with unit tests",
        &["deny", "allow"],
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert_eq!(
        outcome.control_requests, 1,
        "one approval should be emitted"
    );
    assert_eq!(
        outcome.tool_results.len(),
        1,
        "a duplicate approval produced more than one tool result: {:?}",
        outcome.tool_results
    );
    assert!(
        !std::fs::read_to_string(dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "the duplicate allow overrode the deny"
    );
}
