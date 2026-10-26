//! MCP sampling through the real binary: a server asks the session's model for
//! text, allowed only when the host opted in.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const MOCK_SERVER: &str = r#"#!/usr/bin/env python3
import json
import sys

pending = None

def send(obj):
    print(json.dumps(obj), flush=True)

def respond(rid, result):
    send({"jsonrpc": "2.0", "id": rid, "result": result})

def error(rid, code, message):
    send({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": message}})

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    if "method" not in request:
        if request.get("id") == 900 and pending is not None:
            if "result" in request:
                text = request["result"].get("content", {}).get("text", "")
                respond(pending, {"content": [{"type": "text", "text": "sampled:" + text}], "isError": False})
            else:
                message = request.get("error", {}).get("message", "no answer")
                respond(pending, {"content": [{"type": "text", "text": "refused:" + message}], "isError": True})
            pending = None
        continue
    method = request.get("method")
    rid = request.get("id")
    if rid is None:
        continue
    if method == "initialize":
        respond(rid, {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "mock", "version": "1"}})
    elif method == "tools/list":
        respond(rid, {"tools": [{"name": "ask", "description": "ask", "inputSchema": {"type": "object", "properties": {}}}]})
    elif method == "tools/call":
        pending = rid
        send({"jsonrpc": "2.0", "id": 900, "method": "sampling/createMessage", "params": {
            "messages": [{"role": "user", "content": {"type": "text", "text": "What is 2+2?"}}],
            "maxTokens": 32
        }})
    else:
        respond(rid, {})
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

fn mock_server() -> PathBuf {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::temp_dir().join(format!(
        "coderex_sampling_server_{}_{}.py",
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

fn configured_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("workspace");
    let script = mock_server();
    std::fs::write(
        dir.path().join(".mcp.json"),
        serde_json::json!({"mcpServers": {"mock": {"command": script.to_str().unwrap(), "args": []}}})
            .to_string(),
    )
    .expect(".mcp.json");
    (dir, script)
}

#[test]
fn sampling_runs_when_the_host_opts_in() {
    let (dir, script) = configured_workspace();
    let (mut provider, base_url) = start_static_provider();
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
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
        .env("CODEREX_MCP_SAMPLING", "allow")
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
        .write_all(b"call the mock sampling tool\n")
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("sampled:"),
        "the sampling request never reached the model:\n{stdout}"
    );
}

#[test]
fn sampling_without_the_opt_in_is_refused_with_the_variable_name() {
    let (dir, script) = configured_workspace();
    let (mut provider, base_url) = start_static_provider();
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
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
        .write_all(b"call the mock sampling tool\n")
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("refused:") && stdout.contains("CODEREX_MCP_SAMPLING"),
        "the refusal does not name the opt-in:\n{stdout}"
    );
}
