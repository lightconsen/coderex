//! `--output-format stream-json` against the real binary and the static mock
//! provider.
//!
//! The unit tests in `coderex-events` cover the frame types; the in-process
//! REPL test covers the mapping with a mock API client. Neither proves that the
//! *binary* emits the protocol, so this test runs `coderex -p
//! --output-format stream-json` as a child process and parses what a host
//! would actually receive: one JSON object per line, exactly one `result`.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Start the static provider on an OS-assigned port and return the base URL.
///
/// The server announces the bound address on stderr; reading it there avoids
/// guessing a free port and racing another test for it. The reader keeps
/// draining stderr afterwards, because the server logs every request and would
/// block on a full pipe otherwise.
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
        .expect("spawn static-anthropic-server");

    let stderr = child.stderr.take().expect("server stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut announced = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !announced {
                if let Some(address) = line.split("listening on ").nth(1) {
                    let _ = tx.send(address.trim().to_string());
                    announced = true;
                }
            }
            // Keep reading after the announcement: the server logs every
            // request to stderr, and a full pipe would stop it.
        }
    });

    let address = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("static provider did not announce its address");
    (child, format!("http://{address}"))
}

fn run_coderex(cwd: &Path, base_url: &str) -> (String, String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args([
            "-p",
            "--output-format",
            "stream-json",
            "--dangerously-skip-permissions",
        ])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("HOME", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn coderex");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write prompt");

    // Read both pipes concurrently: a full stderr pipe would block the child
    // while this thread is still reading stdout.
    let stdout_pipe = child.stdout.take().expect("stdout");
    let stderr_pipe = child.stderr.take().expect("stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut text = String::new();
        BufReader::new(stdout_pipe)
            .read_to_string(&mut text)
            .expect("read stdout");
        text
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        BufReader::new(stderr_pipe)
            .read_to_string(&mut text)
            .expect("read stderr");
        text
    });

    let status = child.wait().expect("wait for coderex");
    let stdout = stdout_reader.join().expect("stdout reader");
    let stderr = stderr_reader.join().expect("stderr reader");
    (stdout, stderr, status.code().unwrap_or(-1))
}

#[test]
fn stream_json_writes_one_frame_per_line() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let (mut provider, base_url) = start_static_provider();

    let started = Instant::now();
    let (stdout, stderr, code) = run_coderex(workspace.path(), &base_url);
    let _ = provider.kill();
    let _ = provider.wait();

    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the binary did not finish"
    );
    assert_eq!(code, 0, "coderex failed; stderr:\n{stderr}");
    assert!(
        !stdout.trim().is_empty(),
        "stream-json wrote nothing; stderr:\n{stderr}"
    );

    let frames: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("line is not one JSON object ({error}): {line}"))
        })
        .collect();

    let results: Vec<_> = frames
        .iter()
        .filter(|frame| frame["type"] == "result")
        .collect();
    assert_eq!(
        results.len(),
        1,
        "a turn must emit exactly one result; frames: {frames:#?}"
    );
    assert_eq!(results[0]["subtype"], "success");
    assert_eq!(results[0]["is_error"], false);
    assert!(
        results[0]["session_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the result frame must name the session"
    );

    // The protocol reports the turn as it happens; a text answer arrives as
    // streaming frames, not as one printed blob.
    assert!(
        frames.iter().any(|frame| {
            matches!(
                frame["type"].as_str(),
                Some("assistant" | "stream_event" | "user" | "system")
            )
        }),
        "no non-result protocol frame was emitted; frames: {frames:#?}"
    );
}
