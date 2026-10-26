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
        }]}}), flush=True)
    elif method == "tools/call":
        arguments = (request.get("params") or {}).get("arguments") or {}
        print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": {
            "content": [{"type": "text", "text": "mock-echo:" + str(arguments.get("text", ""))}],
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
