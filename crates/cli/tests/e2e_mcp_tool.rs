//! An MCP server's tool, listed and invoked through the real binary.
//!
//! The MCP crate tests the transport; this proves the production wiring: the
//! `.mcp.json` server reaches the model's tool list and a model-issued
//! `mcp__mock__echo` call actually runs in the server process.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const MOCK_SERVER: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method")
    if "id" not in request:
        continue
    rid = request["id"]
    if method == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "1"}
        }}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": {"tools": [{
            "name": "echo",
            "description": "Echo the text",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
        }, {
            "name": "verdict",
            "description": "Refuse the action under review",
            "inputSchema": {"type": "object", "properties": {}}
        }, {
            "name": "image",
            "description": "Return a small PNG",
            "inputSchema": {"type": "object", "properties": {}}
        }, {
            "name": "probe",
            "description": "Try to read a path outside the sandbox grants",
            "inputSchema": {"type": "object", "properties": {}}
        }]}}), flush=True)
    elif method == "tools/call":
        arguments = (request.get("params") or {}).get("arguments") or {}
        name = (request.get("params") or {}).get("name")
        if name == "verdict":
            content = [{"type": "text", "text": json.dumps({"action": "stop"})}]
        elif name == "image":
            content = [{"type": "image", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==", "mimeType": "image/png"}]
        elif name == "probe":
            try:
                with open(os.environ["PROBE_PATH"], "w") as probe:
                    probe.write("probe")
                content = [{"type": "text", "text": "write-ok"}]
            except Exception as error:
                content = [{"type": "text", "text": "write-denied:" + type(error).__name__ + ":" + str(error)}]
        else:
            content = [{"type": "text", "text": "mock-echo:" + str(arguments.get("text", ""))}]
        print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": {
            "content": content,
            "isError": False
        }}), flush=True)
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": {}}), flush=True)
"#;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn start_static_provider() -> (Child, String) {
    start_static_provider_recording(None)
}

fn start_static_provider_recording(record: Option<&Path>) -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut args = vec![
        "--port".to_string(),
        "0".to_string(),
        "--responses".to_string(),
        responses.to_str().expect("responses path").to_string(),
    ];
    if let Some(record) = record {
        args.push("--record".to_string());
        args.push(record.to_str().expect("utf8 record path").to_string());
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args(&args)
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

fn write_mock_server() -> PathBuf {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // Outside the workspace: the MCP spawn path deliberately refuses a
    // command that resolves inside the session's working directory.
    let path = std::env::temp_dir().join(format!(
        "coderex_mock_mcp_e2e_{}_{}.py",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, MOCK_SERVER).expect("write mock server");
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

fn run_coderex(dir: &Path, base_url: &str, extra_args: &[&str]) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args([
            "-p",
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
        ])
        .args(extra_args)
        .current_dir(dir)
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
        .write_all(b"call the mock mcp tool\n")
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

fn configured_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("workspace");
    let script = write_mock_server();
    std::fs::write(
        dir.path().join(".mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "mock": {"command": script.to_str().unwrap(), "args": []}
            }
        })
        .to_string(),
    )
    .unwrap();
    (dir, script)
}

fn assert_echoed(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mock-echo:hi"),
        "the MCP server's output never reached the transcript:\n{stdout}"
    );
    assert!(
        stdout.contains("mcp__mock__echo"),
        "the model-visible name is missing:\n{stdout}"
    );
}

#[test]
fn a_model_tool_call_runs_in_the_configured_mcp_server() {
    let (dir, script) = configured_workspace();
    let (mut provider, base_url) = start_static_provider();
    let output = run_coderex(dir.path(), &base_url, &[]);
    let _ = provider.kill();
    let _ = provider.wait();
    assert_echoed(&output);
    let _ = std::fs::remove_file(&script);
}

#[test]
fn the_ide_flag_connects_to_the_named_mcp_server() {
    let (dir, script) = configured_workspace();
    let (mut provider, base_url) = start_static_provider();
    let output = run_coderex(dir.path(), &base_url, &["--ide", "mock"]);
    let _ = provider.kill();
    let _ = provider.wait();
    assert_echoed(&output);
    let _ = std::fs::remove_file(&script);
}

/// An `mcp_tool` PreToolUse hook asks the configured MCP server and applies
/// its JSON answer: the verdict refuses the Write.
#[test]
fn an_mcp_tool_hook_can_block_the_tool() {
    let dir = tempfile::tempdir().expect("workspace");
    let script = write_mock_server();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "mock": {"command": script.to_str().unwrap(), "args": []}
            }
        })
        .to_string(),
    )
    .unwrap();
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        serde_json::json!({
            "config_version": 4,
            "telemetry_enabled": false,
            "hooks": [{
                "event": "PreToolUse",
                "type": "mcp_tool",
                "if": "Write(*)",
                "server": "mock",
                "tool": "verdict",
                "input": {},
                "timeout_ms": 10000
            }]
        })
        .to_string(),
    )
    .unwrap();
    let (mut provider, base_url) = start_static_provider();

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--dangerously-skip-permissions"])
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
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
        .write_all(b"add a recursive fibonacci function to src/lib.rs with unit tests\n")
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");

    let _ = provider.kill();
    let _ = provider.wait();
    let _ = std::fs::remove_file(&script);
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "an mcp_tool hook that answered stop let the write through:\n{content}"
    );
}

/// An MCP server's image content becomes a provider image block, not just a
/// line of text the model cannot see.
#[test]
fn an_mcp_image_result_reaches_the_provider_as_an_image_block() {
    let dir = tempfile::tempdir().expect("workspace");
    let script = write_mock_server();
    std::fs::write(
        dir.path().join(".mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "mock": {"command": script.to_str().unwrap(), "args": []}
            }
        })
        .to_string(),
    )
    .unwrap();
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .unwrap();
    let record = dir.path().join("requests.jsonl");
    let (mut provider, base_url) = start_static_provider_recording(Some(&record));

    let mut child = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--dangerously-skip-permissions"])
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
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
        .write_all(b"call the mock image tool\n")
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");

    let _ = provider.kill();
    let _ = provider.wait();
    let _ = std::fs::remove_file(&script);
    assert!(
        output.status.success(),
        "coderex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = std::fs::read_to_string(&record).unwrap_or_default();
    let carried_image = requests.lines().any(|line| {
        let body: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
        body.to_string().contains("\"type\":\"image\"")
            && body
                .to_string()
                .contains("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ")
    });
    assert!(
        carried_image,
        "the image never became a provider image block; requests:\n{requests}"
    );
}

/// The MCP server is confined by default; the documented environment switch
/// really lifts the confinement.
#[test]
fn the_mcp_sandbox_escape_hatch_lifts_the_confinement() {
    fn run_with_sandbox(env: Option<&str>) -> String {
        let dir = tempfile::tempdir().expect("workspace");
        let script = write_mock_server();
        let probe_path = format!(
            "/var/tmp/coderex-mcp-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::fs::write(
            dir.path().join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "mock": {
                        "command": script.to_str().unwrap(),
                        "args": [],
                        "env": {"PROBE_PATH": probe_path}
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let _ = std::fs::remove_file(&probe_path);
        let config_home = tempfile::tempdir().expect("config home");
        std::fs::write(
            config_home.path().join("settings.json"),
            r#"{"config_version": 4, "telemetry_enabled": false}"#,
        )
        .unwrap();
        let (mut provider, base_url) = start_static_provider();
        let mut command = Command::new(env!("CARGO_BIN_EXE_coderex"));
        command
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
            .env("CLAUDE_CONFIG_DIR", config_home.path())
            .env("CODEREX_LOG", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match env {
            Some(value) => {
                command.env("CODEREX_MCP_SANDBOX", value);
            }
            None => {
                command.env_remove("CODEREX_MCP_SANDBOX");
            }
        }
        let mut child = command.spawn().expect("spawn coderex");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"call the mock probe tool\n")
            .expect("write prompt");
        let output = child.wait_with_output().expect("wait");
        let _ = provider.kill();
        let _ = provider.wait();
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&probe_path);
        assert!(
            output.status.success(),
            "coderex failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    let sandboxed = run_with_sandbox(None);
    assert!(
        sandboxed.contains("write-denied"),
        "a sandboxed MCP server wrote outside its grants:\n{sandboxed}"
    );
    let unconfined = run_with_sandbox(Some("0"));
    assert!(
        unconfined.contains("write-ok"),
        "CODEREX_MCP_SANDBOX=0 did not lift the confinement:\n{unconfined}"
    );
}
