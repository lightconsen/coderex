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

#[test]
fn the_setup_hook_runs_after_init() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"setup_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let marker = dir.path().join("setup-hit");
    let setup = script(
        dir.path(),
        "setup.sh",
        &format!("printf hit >> {}", marker.display()),
    );
    let home = seed_config(&hook_settings("Setup", None, setup.to_str().unwrap()));
    let (mut provider, base_url) = start_static_provider();

    let output = run(
        dir.path(),
        home.path(),
        &base_url,
        "/init",
        &["--dangerously-skip-permissions"],
    );

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Project initialized"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(&marker).expect("the Setup hook ran"),
        "hit"
    );
}

/// A provider that keeps the turn open long enough for a file change to land.
struct SlowProvider {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SlowProvider {
    fn start() -> Self {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow provider");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let mut header_end = None;
                while header_end.is_none() {
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            header_end = buffer
                                .windows(4)
                                .position(|window| window == b"\r\n\r\n")
                                .map(|p| p + 4);
                        }
                    }
                }
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while buffer.len() < header_end + length {
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                }
                if socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                    )
                    .is_err()
                {
                    continue;
                }
                let _ = socket.write_all(
                    b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                );
                for _ in 0..80 {
                    if stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    if socket.write_all(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"tick \"}}\n\n").is_err() {
                        break;
                    }
                    let _ = socket.flush();
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        });
        Self {
            url,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for SlowProvider {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn watch_paths_from_session_start_arm_file_changed() {
    let dir = tempfile::tempdir().expect("workspace");
    let watched = dir.path().join("watched.txt");
    std::fs::write(&watched, "before\n").unwrap();
    let marker = dir.path().join("file-changed-hit");
    let session_start = script(
        dir.path(),
        "session-start.sh",
        &format!(
            "printf '%s\\n' '{{\"action\":\"continue\",\"watchPaths\":[\"{}\"]}}'",
            watched.display()
        ),
    );
    let changed = script(
        dir.path(),
        "file-changed.sh",
        &format!("printf hit >> {}", marker.display()),
    );
    let settings = serde_json::json!({
        "hooks": [
            {
                "event": "SessionStart",
                "command": session_start.to_str().unwrap(),
                "timeout_ms": 5000,
                "sandbox": {"enabled": false}
            },
            {
                "event": "FileChanged",
                "command": changed.to_str().unwrap(),
                "timeout_ms": 5000,
                "sandbox": {"enabled": false}
            }
        ]
    });
    let home = seed_config(&settings);
    let provider = SlowProvider::start();
    let child = run_spawned(
        dir.path(),
        home.path(),
        &provider.url,
        "write a very long response",
        &["--dangerously-skip-permissions"],
    );

    for tick in 1..=4 {
        std::thread::sleep(Duration::from_secs(2));
        std::fs::write(&watched, format!("after-{tick}\n")).unwrap();
    }

    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(provider);
    let hits = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        hits.contains("hit"),
        "FileChanged did not fire for a SessionStart watch path\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_spawned(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    prompt: &str,
    args: &[&str],
) -> std::process::Child {
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
    child
}

/// A tiny HTTP endpoint that answers every hook request with `stop`.
struct HookHttpServer {
    url: String,
    body: std::sync::Arc<std::sync::Mutex<String>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HookHttpServer {
    fn start() -> Self {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hook server");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}/hook", listener.local_addr().expect("addr"));
        let body = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let body_thread = std::sync::Arc::clone(&body);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                let mut header_end = None;
                while header_end.is_none() {
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            header_end = buffer
                                .windows(4)
                                .position(|window| window == b"\r\n\r\n")
                                .map(|position| position + 4);
                        }
                    }
                }
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while buffer.len() < header_end + length {
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                    }
                }
                *body_thread.lock().unwrap() =
                    String::from_utf8_lossy(&buffer[header_end..header_end + length]).into_owned();
                let response = r#"{"action":"stop","additional_context":"http hook"}"#;
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                        response.len()
                    )
                    .as_bytes(),
                );
            }
        });
        Self {
            url,
            body,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for HookHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn an_http_pretooluse_hook_can_block_the_tool() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let server = HookHttpServer::start();
    let settings = serde_json::json!({
        "hooks": [{
            "event": "PreToolUse",
            "type": "http",
            "if": "Write(*)",
            "url": server.url,
            "timeout_ms": 5000
        }]
    });
    let home = seed_config(&settings);
    let api = start_static_provider();

    let output = run(
        dir.path(),
        home.path(),
        &api.1,
        "add a recursive fibonacci function to src/lib.rs with unit tests",
        &["--dangerously-skip-permissions"],
    );

    let _ = api.0;
    let body = server.body.lock().unwrap().clone();
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        body.contains("\"Write\""),
        "the hook did not receive the tool call: {body}"
    );
    let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "an HTTP hook that answered stop let the write through:\n{content}"
    );
}

/// Model-backed hooks run through the real session's action runner: the
/// prompt reaches the provider and the JSON answer stops the tool.
#[test]
fn a_model_backed_pretooluse_hook_can_block_the_tool() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let home = seed_config(&serde_json::json!({
        "hooks": [{
            "event": "PreToolUse",
            "type": "prompt",
            "if": "Write(*)",
            "prompt": "Give a hook verdict for this tool call",
            "timeout_ms": 10000
        }]
    }));
    let api = start_static_provider();

    let output = run(
        dir.path(),
        home.path(),
        &api.1,
        "add a recursive fibonacci function to src/lib.rs with unit tests",
        &["--dangerously-skip-permissions"],
    );

    let _ = api.0;
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "a prompt hook that answered stop let the write through:\n{content}"
    );
}

/// `agent` hooks are a beta: without the flag the session skips them (and says
/// so), and with it the verifier's answer gates the tool.
#[test]
fn an_agent_hook_is_opt_in_and_can_block_the_tool() {
    let agent_home = || {
        seed_config(&serde_json::json!({
            "hooks": [{
                "event": "PreToolUse",
                "type": "agent",
                "if": "Write(*)",
                "prompt": "Give a hook verdict for this tool call",
                "timeout_ms": 10000
            }]
        }))
    };
    let prompt = "add a recursive fibonacci function to src/lib.rs with unit tests";

    let without = tempfile::tempdir().expect("workspace without the flag");
    std::fs::create_dir_all(without.path().join("src")).unwrap();
    std::fs::write(
        without.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let home = agent_home();
    let api = start_static_provider();
    let skipped = run(
        without.path(),
        home.path(),
        &api.1,
        prompt,
        &["--dangerously-skip-permissions"],
    );
    let _ = api.0;
    assert!(skipped.status.success(), "{skipped:?}");
    assert!(
        String::from_utf8_lossy(&skipped.stderr).contains("agent_triggers"),
        "the skipped agent hook did not name its flag: {}",
        String::from_utf8_lossy(&skipped.stderr)
    );
    assert!(
        std::fs::read_to_string(without.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "an un-enabled agent hook still gated the write"
    );

    let with = tempfile::tempdir().expect("workspace with the flag");
    std::fs::create_dir_all(with.path().join("src")).unwrap();
    std::fs::write(
        with.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let home = agent_home();
    let api = start_static_provider();
    let gated = run(
        with.path(),
        home.path(),
        &api.1,
        prompt,
        &[
            "--dangerously-skip-permissions",
            "--betas",
            "agent_triggers",
        ],
    );
    let _ = api.0;
    assert!(
        gated.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&gated.stderr)
    );
    let content = std::fs::read_to_string(with.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "an enabled agent hook that answered stop let the write through:\n{content}"
    );
}

/// Every lifecycle event a headless session can reach really reaches a hook,
/// with the event name in the hook's stdin JSON. A wildcard hook records the
/// name and answers `continue` so the session proceeds.
#[test]
fn the_session_lifecycle_events_reach_their_hooks() {
    fn marker_script(dir: &Path, marker: &Path) -> PathBuf {
        let body = concat!(
            "input=$(cat)\n",
            "printf '%s\\n' \"$input\" | sed -n 's/.*\"event\":\"\\([^\"]*\\)\".*/\\1/p' >> ",
            "MARKER",
            "\n",
            "echo '{\"action\":\"continue\"}'\n",
        )
        .replace("MARKER", &marker.display().to_string());
        script(dir, "record-event.sh", &body)
    }

    fn recorded(marker: &Path) -> Vec<String> {
        std::fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    let prompt = "add a recursive fibonacci function to src/lib.rs with unit tests";

    // Without a grant, Write asks; the request and its failure are both hooks.
    let asking = tempfile::tempdir().expect("asking workspace");
    std::fs::create_dir_all(asking.path().join("src")).unwrap();
    std::fs::write(
        asking.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let marker = asking.path().join("events");
    let recorder = marker_script(asking.path(), &marker);
    let home = seed_config(&serde_json::json!({
        "hooks": [{
            "event": "*",
            "command": recorder.display().to_string(),
            "timeout_ms": 5000,
            "sandbox": {"enabled": false}
        }]
    }));
    let api = start_static_provider();
    let denied = run(asking.path(), home.path(), &api.1, prompt, &[]);
    let _ = api.0;
    assert!(denied.status.success(), "{denied:?}");
    let events = recorded(&marker);
    for expected in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUseFailure",
        "TurnEnd",
        "Stop",
    ] {
        assert!(
            events.iter().any(|event| event == expected),
            "the {expected} hook did not fire; saw {events:?}"
        );
    }
    assert!(
        !events.iter().any(|event| event == "PostToolUse"),
        "a denied Write reported PostToolUse: {events:?}"
    );
    assert!(
        !std::fs::read_to_string(asking.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "the denied write landed"
    );

    // With the grant, the same session runs the tool and reports PostToolUse.
    let granted = tempfile::tempdir().expect("granted workspace");
    std::fs::create_dir_all(granted.path().join("src")).unwrap();
    std::fs::write(
        granted.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let marker = granted.path().join("events");
    let recorder = marker_script(granted.path(), &marker);
    let home = seed_config(&serde_json::json!({
        "hooks": [{
            "event": "*",
            "command": recorder.display().to_string(),
            "timeout_ms": 5000,
            "sandbox": {"enabled": false}
        }]
    }));
    let api = start_static_provider();
    let allowed = run(
        granted.path(),
        home.path(),
        &api.1,
        prompt,
        &["--dangerously-skip-permissions"],
    );
    let _ = api.0;
    assert!(allowed.status.success(), "{allowed:?}");
    let events = recorded(&marker);
    for expected in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "TurnEnd",
        "Stop",
    ] {
        assert!(
            events.iter().any(|event| event == expected),
            "the {expected} hook did not fire; saw {events:?}"
        );
    }
    assert!(
        std::fs::read_to_string(granted.path().join("src/lib.rs"))
            .unwrap()
            .contains("fibonacci"),
        "the granted write did not land"
    );
}

/// Hooks that fire from inside a tool (`TaskCompleted`, `WorktreeCreate`) and
/// from a sub-agent (`SubagentStart`, `SubagentStop`, `TeammateIdle`) reach
/// their configured hooks through the real binary.
///
/// The engine only fires the hooks it owns. These events are produced by the
/// tool dispatcher and the agent manager, which each hold their own executor;
/// when the CLI installed the registry on the engine alone, a hook configured
/// here loaded, matched, and never ran.
#[test]
fn tool_and_subagent_hooks_reach_their_configured_commands() {
    fn marker_script(dir: &Path, marker: &Path) -> PathBuf {
        let body = concat!(
            "input=$(cat)\n",
            "printf '%s\\n' \"$input\" | sed -n 's/.*\"event\":\"\\([^\"]*\\)\".*/\\1/p' >> ",
            "MARKER",
            "\n",
            "echo '{\"action\":\"continue\"}'\n",
        )
        .replace("MARKER", &marker.display().to_string());
        script(dir, "record-event.sh", &body)
    }

    fn recorded(marker: &Path) -> Vec<String> {
        std::fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn run_with_marker(prompt: &str, args: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        // `EnterWorktree` creates a real git worktree, so the fixture has to
        // be a repository with at least one commit.
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.email=test@example.test",
                "-c",
                "user.name=Test",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            let status = Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        }
        let marker = dir.path().join("events");
        let recorder = marker_script(dir.path(), &marker);
        let home = seed_config(&serde_json::json!({
            "hooks": [{
                "event": "*",
                "command": recorder.display().to_string(),
                "timeout_ms": 10000,
                "sandbox": {"enabled": false}
            }]
        }));
        let api = start_static_provider();
        let output = run(dir.path(), home.path(), &api.1, prompt, args);
        let _ = api.0;
        assert!(
            output.status.success(),
            "coderex failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        recorded(&marker)
    }

    let task = run_with_marker(
        "create a test task and then complete the smoke task",
        &["--dangerously-skip-permissions"],
    );
    assert!(
        task.iter().any(|event| event == "TaskCompleted"),
        "TaskCompleted never fired; saw {task:?}"
    );

    let worktree = run_with_marker("enter a worktree", &["--dangerously-skip-permissions"]);
    assert!(
        worktree.iter().any(|event| event == "WorktreeCreate"),
        "WorktreeCreate never fired; saw {worktree:?}"
    );
    // `--worktree` moves the session during startup, before the hooks are
    // loaded; the CLI replays that one move once they are installed.
    let moved = run_with_marker("hello", &["--dangerously-skip-permissions", "--worktree"]);
    assert!(
        moved.iter().any(|event| event == "CwdChanged"),
        "CwdChanged never fired for --worktree; saw {moved:?}"
    );

    let subagent = run_with_marker(
        "delegate a simple agent",
        &["--dangerously-skip-permissions"],
    );
    for expected in ["SubagentStart", "SubagentStop", "TeammateIdle"] {
        assert!(
            subagent.iter().any(|event| event == expected),
            "the {expected} hook never fired; saw {subagent:?}"
        );
    }
}

/// The events that only a real failure or a compaction produces: a denied
/// tool, a provider outage, and the two ends of a compaction.
///
/// These four names existed in the engine's vocabulary with no binary
/// evidence. `PermissionDenied` in particular is easy to confuse with
/// `PostToolUseFailure` — a rule refusal fires both, and only the former names
/// the rule.
#[test]
fn failure_and_compaction_events_reach_their_hooks() {
    fn recorder(dir: &Path, marker: &Path) -> PathBuf {
        let body = concat!(
            "input=$(cat)\n",
            "printf '%s\\n' \"$input\" | sed -n 's/.*\"event\":\"\\([^\"]*\\)\".*/\\1/p' >> ",
            "MARKER",
            "\n",
            "echo '{\"action\":\"continue\"}'\n",
        )
        .replace("MARKER", &marker.display().to_string());
        script(dir, "record-event.sh", &body)
    }

    fn recorded(marker: &Path) -> Vec<String> {
        std::fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    // 1. A deny rule refuses the tool: PermissionDenied fires with the rule's
    //    reason, and PostToolUseFailure fires because the call failed.
    let denied_dir = tempfile::tempdir().expect("denied workspace");
    std::fs::create_dir_all(denied_dir.path().join(".coderex")).unwrap();
    let denied_marker = denied_dir.path().join("events");
    let hook = recorder(denied_dir.path(), &denied_marker);
    std::fs::write(
        denied_dir.path().join(".coderex/settings.json"),
        serde_json::json!({
            "permissions": {"deny": ["Bash(echo:*)"]},
        })
        .to_string(),
    )
    .unwrap();
    let home = seed_config(&serde_json::json!({
        "hooks": [{
            "event": "*",
            "command": hook.display().to_string(),
            "timeout_ms": 10000,
            "sandbox": {"enabled": false}
        }]
    }));
    let api = start_static_provider();
    let output = run(
        denied_dir.path(),
        home.path(),
        &api.1,
        "run a safe echo",
        &[],
    );
    let _ = api.0;
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = recorded(&denied_marker);
    for expected in ["PermissionDenied", "PostToolUseFailure"] {
        assert!(
            events.iter().any(|event| event == expected),
            "the {expected} hook never fired; saw {events:?}"
        );
    }

    // 2. The Notification hook is not exercised here: it is fired by the
    //    engine's retry-exhausted path, which the streaming submission the
    //    CLI uses cannot reach (`send_request_streaming` turns a failed stream
    //    into `TurnError::Fatal` instead of an `ApiError`). It is covered at
    //    the engine level, where the blocking client's `RetryExhausted` is
    //    reachable.

    // 3. A compaction is bracketed by PreCompact/PostCompact. The tiny window
    //    makes the first request over the compaction line.
    let compact_dir = tempfile::tempdir().expect("compact workspace");
    let compact_marker = compact_dir.path().join("events");
    let hook = recorder(compact_dir.path(), &compact_marker);
    let home = seed_config(&serde_json::json!({
        "hooks": [{
            "event": "*",
            "command": hook.display().to_string(),
            "timeout_ms": 10000,
            "sandbox": {"enabled": false}
        }]
    }));
    let api = start_static_provider();
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--dangerously-skip-permissions"])
        .current_dir(compact_dir.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &api.1)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODEREX_LOG", "off")
        .env("CODEREX_CONTEXT_WINDOW", "1000")
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
    let output = child.wait_with_output().expect("wait");
    let _ = api.0;
    let _ = output;
    let events = recorded(&compact_marker);
    for expected in ["PreCompact", "PostCompact"] {
        assert!(
            events.iter().any(|event| event == expected),
            "the {expected} hook never fired; saw {events:?}"
        );
    }
}
