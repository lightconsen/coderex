//! Integration tests for the stdio spawn path's sandbox decisions.
//!
//! The policy helpers are unit-tested in `src/sandbox.rs`; these drive
//! `McpServer::spawn`, which is where a policy becomes a wrapper and where the
//! "does the command exist?" check has to happen first.

use std::collections::HashMap;

use coderex_mcp::server::McpServer;

/// A server whose command does not exist must fail with the command error,
/// not with a wrapper/protocol error that hides the real problem.
#[tokio::test]
async fn a_missing_command_fails_before_the_sandbox_wrapper() {
    let mut server = McpServer::new(
        "missing",
        "/nonexistent/coderex-mcp-binary",
        vec![],
        HashMap::new(),
    );
    server.sandbox.enabled = true;

    let error = server
        .spawn()
        .await
        .expect_err("the command does not exist");
    let message = error.to_string();

    assert!(
        message.contains("could not be started"),
        "expected the command error, got: {message}"
    );
    assert!(
        !message.to_lowercase().contains("sandbox"),
        "the sandbox must not be blamed for a missing command: {message}"
    );
}

#[tokio::test]
async fn disabling_the_sandbox_does_not_change_the_missing_command_error() {
    let mut server = McpServer::new(
        "missing",
        "/nonexistent/coderex-mcp-binary",
        vec![],
        HashMap::new(),
    );
    server.sandbox.enabled = false;

    let error = server
        .spawn()
        .await
        .expect_err("the command does not exist");
    assert!(
        error.to_string().contains("could not be started"),
        "{error}"
    );
}

/// A command that lands inside the working directory is a checkout deciding
/// what the client launches; it is refused regardless of the sandbox policy.
#[cfg(unix)]
#[tokio::test]
async fn a_command_inside_the_working_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let cwd = std::env::current_dir().expect("cwd");
    let path = cwd
        .join("target")
        .join(format!("coderex-local-server-{}.sh", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap()).expect("target dir");
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write local command");
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();

    let mut server = McpServer::new(
        "local",
        path.to_str().expect("utf-8 path"),
        vec![],
        HashMap::new(),
    );
    server.sandbox.enabled = false;

    let error = server
        .spawn()
        .await
        .expect_err("local commands are refused");
    let message = error.to_string();
    let _ = std::fs::remove_file(&path);
    assert!(
        message.contains("refusing to spawn"),
        "expected the cwd refusal, got: {message}"
    );
}

/// With the sandbox explicitly on, a real server still starts and answers.
#[cfg(unix)]
#[tokio::test]
async fn an_explicitly_sandboxed_server_starts_and_lists_tools() {
    let script =
        std::env::temp_dir().join(format!("coderex_sandbox_spawn_{}.py", std::process::id()));
    std::fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if "id" not in request:
        continue
    if method == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "sandboxed", "version": "1"}
        }}), flush=True)
    elif method == "tools/list":
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {
            "tools": [{"name": "ping", "description": "p", "inputSchema": {"type": "object"}}]
        }}), flush=True)
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {}}), flush=True)
"#,
    )
    .expect("write mock");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
    }

    let mut server = McpServer::new(
        "sandboxed",
        script.to_str().unwrap(),
        vec![],
        HashMap::new(),
    );
    server.sandbox.enabled = true;

    server.spawn().await.expect("the sandboxed server starts");
    let tools = server.discover_tools().await.expect("tools/list");
    assert_eq!(tools["tools"][0]["name"], "ping");
    server.stop().await;

    let _ = std::fs::remove_file(&script);
}
