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
