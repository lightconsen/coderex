//! MCP integration tests — covering server lifecycle, tool/resource discovery,
//! OAuth, and connection management with a mock JSON-RPC stdio server.
//!
//! These tests correspond to the equivalence test plan Module 11:
//! 11.1 server lifecycle, 11.2 tool discovery, 11.3 resource reading,
//! 11.4 OAuth, 11.5 connection management

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use coderex_core::tool_executor::ToolExecutor;
use coderex_mcp::channel::JsonRpcChannel;
use coderex_mcp::config::{McpConfig, McpServerConfig};
use coderex_mcp::manager::McpManager;
use coderex_mcp::oauth::{McpOAuthState, OAuthManager};
use coderex_mcp::proxy::McpToolProxy;
use coderex_mcp::resources::{McpResource, McpResourceRegistry};
use coderex_mcp::result_adapter::McpResultAdapter;
use coderex_mcp::server::{McpServer, ServerStatus};
use coderex_tools::{
    ExecutionConfig, ExecutionContext, RegistryToolDispatcher, ToolExecutor as ToolsExecutor,
    ToolRegistry,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Mock MCP Server Script (Python)
// ═══════════════════════════════════════════════════════════════════════════

static MOCK_ID: AtomicU64 = AtomicU64::new(0);

const MOCK_MCP_SERVER: &str = r#"#!/usr/bin/env python3
import json
import sys

def send_response(req_id, result):
    print(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result}), flush=True)

def send_error(req_id, code, message):
    print(json.dumps({"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        continue

    method = req.get("method", "")
    req_id = req.get("id")

    if method == "initialize":
        send_response(req_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}, "resources": {}},
            "serverInfo": {"name": "mock-mcp", "version": "1.0"}
        })
    elif method == "initialized":
        pass  # notification, no response
    elif method == "tools/list":
        send_response(req_id, {
            "tools": [
                {
                    "name": "mock_read_file",
                    "description": "Read a file from the filesystem",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }
                },
                {
                    "name": "mock_write_file",
                    "description": "Write content to a file",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["path", "content"]
                    }
                }
            ]
        })
    elif method == "resources/list":
        send_response(req_id, {
            "resources": [
                {
                    "uri": "file:///tmp/test.txt",
                    "name": "test.txt",
                    "mimeType": "text/plain",
                    "description": "A test file"
                },
                {
                    "uri": "file:///tmp/data.json",
                    "name": "data.json",
                    "mimeType": "application/json",
                    "description": "JSON data"
                }
            ]
        })
    elif method == "resources/read":
        params = req.get("params", {})
        uri = params.get("uri", "")
        if uri == "file:///tmp/test.txt":
            send_response(req_id, {
                "contents": [
                    {
                        "uri": uri,
                        "mimeType": "text/plain",
                        "text": "hello from resource"
                    }
                ]
            })
        elif uri == "file:///tmp/data.json":
            send_response(req_id, {
                "contents": [
                    {
                        "uri": uri,
                        "mimeType": "application/json",
                        "blob": "eyJvayI6dHJ1ZX0="
                    }
                ]
            })
        elif uri == "file:///tmp/empty":
            send_response(req_id, {"contents": []})
        elif uri == "file:///tmp/unsupported":
            send_response(req_id, {
                "contents": [{"uri": uri, "mimeType": "application/octet-stream"}]
            })
        elif uri == "file:///tmp/boom":
            send_error(req_id, -32002, "resource unavailable")
        else:
            send_error(req_id, -32002, f"resource not found: {uri}")
    elif method == "tools/call":
        params = req.get("params", {})
        tool_name = params.get("name", "")
        args = params.get("arguments", {})
        send_response(req_id, {
            "content": [{"type": "text", "text": f"Executed {tool_name} with {json.dumps(args)}"}],
            "isError": False
        })
    else:
        send_error(req_id, -32601, f"Method {method} not found")
"#;

fn mock_server_path() -> PathBuf {
    let id = MOCK_ID.fetch_add(1, Ordering::SeqCst);
    let tmp = std::env::temp_dir();
    // The counter restarts in every test process, so the pid is part of the
    // name: two concurrently running nextest processes would otherwise write
    // and exec the same file, and exec can race the write with ETXTBSY.
    let path = tmp.join(format!(
        "coderex_mock_mcp_server_{}_{id}.py",
        std::process::id()
    ));
    std::fs::write(&path, MOCK_MCP_SERVER).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// A started manager whose single `mock` server is the Python mock above.
async fn running_mock_manager() -> McpManager {
    let script = mock_server_path();
    let mgr = McpManager::new();
    let mut config = McpConfig::new();
    config.add_server(
        "mock",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
        },
    );
    mgr.load_config(&config);
    mgr.start_all().await;
    mgr
}

// ═══════════════════════════════════════════════════════════════════════════
//  11.1 Server Lifecycle — spawn, health check, stop, status transitions
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn server_lifecycle_spawn_and_stop() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());

    assert_eq!(server.status, ServerStatus::Stopped);

    let result: Result<(), _> = server.spawn().await;
    result.unwrap();
    assert_eq!(server.status, ServerStatus::Running);
    assert!(server.channel().is_some());

    server.stop().await;
    assert_eq!(server.status, ServerStatus::Stopped);
    assert!(server.channel().is_none());
}

#[tokio::test]
async fn server_lifecycle_health_check_when_running() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());

    server.spawn().await.unwrap();
    let result: Result<(), _> = server.health_check().await;
    assert!(result.is_ok(), "health check failed: {result:?}");

    server.stop().await;
}

#[tokio::test]
async fn server_lifecycle_health_check_fails_when_stopped() {
    let script = mock_server_path();
    let server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());

    let result: Result<(), _> = server.health_check().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn server_lifecycle_spawn_already_running_errors() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());

    server.spawn().await.unwrap();
    let result: Result<(), _> = server.spawn().await;
    assert!(result.is_err());

    server.stop().await;
}

// ═══════════════════════════════════════════════════════════════════════════
//  11.2 Tool Discovery — tools/list, registration, proxy call
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn tool_discovery_discover_tools() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());
    server.spawn().await.unwrap();

    let result: Result<serde_json::Value, _> = server.discover_tools().await;
    assert!(result.is_ok(), "discover_tools failed: {result:?}");

    let value = result.unwrap();
    let tools = value.get("tools").and_then(|t| t.as_array());
    assert!(tools.is_some());
    assert_eq!(tools.unwrap().len(), 2);

    server.stop().await;
}

#[tokio::test]
async fn tool_discovery_manager_discovers_and_registers() {
    let script = mock_server_path();
    let mgr = McpManager::new();
    let mut config = McpConfig::new();
    config.add_server(
        "mock",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
        },
    );
    mgr.load_config(&config);

    mgr.start_all().await;

    assert!(mgr.is_running("mock"));
    assert_eq!(mgr.tool_count(), 2);
    let tools = mgr.list_tools();
    assert!(tools.contains(&"mcp__mock__mock_read_file".to_string()));
    assert!(tools.contains(&"mcp__mock__mock_write_file".to_string()));

    mgr.stop_all().await;
}

/// A server that asks the client a question.
///
/// The protocol runs both ways: `elicitation/create` is the server asking the
/// client to ask the person. The client used to parse every line as a response
/// and drop anything whose id it had not sent, so the request vanished and the
/// server waited for an answer that never came.
const ASKING_SERVER: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

def send(obj):
    print(json.dumps(obj), flush=True)

def log(obj):
    # What the client answered, for the test to read.
    path = os.environ.get("CODEREX_MOCK_ANSWER_LOG")
    if path:
        with open(path, "a") as handle:
            handle.write(json.dumps(obj) + "\n")

# The request the server owes an answer to, while it waits for the client.
waiting = {}

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    req_id = req.get("id")

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": req_id, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}, "elicitation": {}},
            "serverInfo": {"name": "asking-mcp", "version": "1.0"}}})
    elif method == "tools/list":
        # Ask the client something first, and remember what we owe an answer.
        waiting[9001] = req_id
        send({"jsonrpc": "2.0", "id": 9001, "method": "elicitation/create",
              "params": {"message": "which environment?"}})
    elif req_id in waiting:
        # The client's answer to the question: whatever it was, the request
        # that was waiting on it can now be answered.
        log({"received": req, "waiting_for": waiting[req_id]})
        send({"jsonrpc": "2.0", "id": waiting.pop(req_id), "result": {"tools": []}})
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": req_id, "result": {}})
"#;

fn asking_server_path() -> PathBuf {
    let id = MOCK_ID.fetch_add(1, Ordering::SeqCst);
    // Same per-process counter caveat as `mock_server_path`: include the pid.
    let path =
        std::env::temp_dir().join(format!("coderex_asking_mcp_{}_{id}.py", std::process::id()));
    std::fs::write(&path, ASKING_SERVER).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// A handler that answers everything, recording what it was asked.
#[derive(Debug)]
struct RecordingHandler {
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl coderex_mcp::channel::InboundRequestHandler for RecordingHandler {
    async fn handle(
        &self,
        method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, coderex_mcp::channel::JsonRpcError> {
        self.asked.lock().unwrap().push(method.to_string());
        Ok(serde_json::json!({"action": "accept", "content": {"environment": "test"}}))
    }
}

/// A handler that waits before answering, standing in for the user's turn at
/// the keyboard.
#[derive(Debug)]
struct SlowHandler {
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    answer: serde_json::Value,
    delay: std::time::Duration,
}

#[async_trait::async_trait]
impl coderex_mcp::channel::InboundRequestHandler for SlowHandler {
    async fn handle(
        &self,
        method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, coderex_mcp::channel::JsonRpcError> {
        self.asked.lock().unwrap().push(method.to_string());
        tokio::time::sleep(self.delay).await;
        Ok(self.answer.clone())
    }
}

fn answer_log() -> PathBuf {
    // As with the mock scripts, the counter is per process: include the pid so
    // two nextest processes cannot share one answer log.
    let log = std::env::temp_dir().join(format!(
        "coderex_mock_answers_{}_{}.jsonl",
        std::process::id(),
        MOCK_ID.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_file(&log);
    log
}

fn answer_env(log: &std::path::Path) -> HashMap<String, String> {
    HashMap::from([(
        "CODEREX_MOCK_ANSWER_LOG".to_string(),
        log.to_string_lossy().into_owned(),
    )])
}

/// The answer the mock server logged, as the client sent it.
fn logged_answer(log: &std::path::Path) -> serde_json::Value {
    let logged = std::fs::read_to_string(log).unwrap_or_default();
    let line = logged.lines().next().unwrap_or_else(|| {
        panic!(
            "the server never received an answer (log {})",
            log.display()
        )
    });
    let entry: serde_json::Value =
        serde_json::from_str(line).expect("the server logged valid JSON");
    entry
        .get("received")
        .cloned()
        .expect("the log entry carries the received message")
}

#[tokio::test]
async fn an_accepting_handler_answers_the_server_and_the_request_completes() {
    let script = asking_server_path();
    let log = answer_log();
    let mut server = McpServer::new("asking", script.to_str().unwrap(), vec![], answer_env(&log));

    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    server.set_inbound_handler(std::sync::Arc::new(RecordingHandler {
        asked: std::sync::Arc::clone(&asked),
    }));

    server.spawn().await.unwrap();

    // The server asks mid-request; the client answers, and the request that
    // was in flight still completes.
    let tools = server.discover_tools().await.unwrap();
    assert!(tools.get("tools").is_some(), "{tools}");

    let asked = asked.lock().unwrap().clone();
    assert!(
        asked.iter().any(|method| method == "elicitation/create"),
        "the client never answered the server's request: {asked:?}"
    );

    let answer = logged_answer(&log);
    assert_eq!(answer["id"], 9001, "the answer used the wrong id: {answer}");
    assert_eq!(answer["result"]["action"], "accept", "{answer}");
    assert_eq!(
        answer["result"]["content"]["environment"], "test",
        "{answer}"
    );

    let _ = std::fs::remove_file(&log);
    server.stop().await;
}

#[tokio::test]
async fn a_slow_decline_is_awaited_and_reaches_the_server() {
    let script = asking_server_path();
    let log = answer_log();
    let mut server = McpServer::new("asking", script.to_str().unwrap(), vec![], answer_env(&log));

    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    server.set_inbound_handler(std::sync::Arc::new(SlowHandler {
        asked: std::sync::Arc::clone(&asked),
        answer: serde_json::json!({"action": "decline"}),
        delay: std::time::Duration::from_millis(100),
    }));

    server.spawn().await.unwrap();

    let tools = server.discover_tools().await.unwrap();
    assert!(tools.get("tools").is_some(), "{tools}");

    let answer = logged_answer(&log);
    assert_eq!(
        answer["result"]["action"], "decline",
        "the handler's decline did not reach the server: {answer}"
    );

    let _ = std::fs::remove_file(&log);
    server.stop().await;
}

#[tokio::test]
async fn a_request_from_the_server_with_no_handler_is_refused_not_ignored() {
    let script = asking_server_path();
    let log = answer_log();
    let mut server = McpServer::new("asking", script.to_str().unwrap(), vec![], answer_env(&log));
    server.spawn().await.unwrap();

    // No handler was set: the client must still answer — the server is waiting
    // — and it must answer with a refusal, not with a success that says the
    // question was answered by nobody.
    let tools = server.discover_tools().await;
    assert!(
        tools.is_ok(),
        "the server never got an answer, so the request never completed: {tools:?}"
    );

    let answer = logged_answer(&log);
    assert_eq!(
        answer
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_i64),
        Some(-32601),
        "a request nothing can answer should be refused, not answered with nothing: {answer}"
    );

    let _ = std::fs::remove_file(&log);
    server.stop().await;
}

#[tokio::test]
async fn tool_discovery_proxy_call() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());
    server.spawn().await.unwrap();

    let channel = server.channel().unwrap();
    let proxy = McpToolProxy::with_channel("test", "mock_read_file", channel);

    let result = proxy
        .call(serde_json::json!({"path": "/tmp/test.txt"}))
        .await;
    assert!(result.is_ok(), "proxy call failed: {result:?}");

    let value = result.unwrap();
    let content = value.get("content").and_then(|c| c.as_array());
    assert!(content.is_some());
    assert_eq!(content.unwrap().len(), 1);

    server.stop().await;
}

#[tokio::test]
async fn discovered_tools_run_through_the_registry_dispatcher() {
    let script = mock_server_path();
    let mgr = std::sync::Arc::new(McpManager::new());
    let mut config = McpConfig::new();
    config.add_server(
        "mock",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
        },
    );
    mgr.load_config(&config);
    mgr.start_all().await;
    mgr.discover_all_tools().await;

    // Registering the discovered tools is all it takes: from here they are
    // ordinary registry tools with no MCP-specific execution path.
    let registry = ToolRegistry::new();
    mgr.register_tools_into(&registry);
    assert!(registry.get_tool_arc("mcp__mock__mock_read_file").is_some());

    let dispatcher = RegistryToolDispatcher::new(
        std::sync::Arc::new(registry),
        std::sync::Arc::new(ToolsExecutor::new(ExecutionConfig::default())),
        ExecutionContext::Normal,
        PathBuf::from("/tmp"),
    );

    let outcome = dispatcher
        .execute(
            "tc_1",
            "mcp__mock__mock_read_file",
            &serde_json::json!({"path": "/tmp/test.txt"}),
        )
        .await;

    assert!(!outcome.result.is_error, "{:?}", outcome.result.output);
    assert!(outcome.result.output.contains("Executed mock_read_file"));

    mgr.stop_all().await;
}

// ═══════════════════════════════════════════════════════════════════════════
//  11.3 Resource Discovery — resources/list, registration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn resource_discovery_discover_resources() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());
    server.spawn().await.unwrap();

    let result: Result<serde_json::Value, _> = server.discover_resources().await;
    assert!(result.is_ok(), "discover_resources failed: {result:?}");

    let value = result.unwrap();
    let resources = value.get("resources").and_then(|r| r.as_array());
    assert!(resources.is_some());
    assert_eq!(resources.unwrap().len(), 2);

    server.stop().await;
}

#[tokio::test]
async fn resource_discovery_manager_discovers_and_registers() {
    let script = mock_server_path();
    let mgr = McpManager::new();
    let mut config = McpConfig::new();
    config.add_server(
        "mock",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
        },
    );
    mgr.load_config(&config);

    mgr.start_all().await;

    assert_eq!(mgr.resource_count(), 2);
    let resources = mgr.list_resources();
    assert!(resources.contains(&"file:///tmp/test.txt".to_string()));
    assert!(resources.contains(&"file:///tmp/data.json".to_string()));

    mgr.stop_all().await;
}

#[tokio::test]
async fn resource_read_server_returns_raw_contents() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());
    server.spawn().await.unwrap();

    let value = server.read_resource("file:///tmp/test.txt").await.unwrap();
    let contents = value.get("contents").and_then(|c| c.as_array()).unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["uri"], "file:///tmp/test.txt");
    assert_eq!(contents[0]["text"], "hello from resource");

    server.stop().await;
}

#[tokio::test]
async fn resource_read_text_is_returned_and_cached() {
    let mgr = running_mock_manager().await;

    let content = mgr
        .read_resource("mock", "file:///tmp/test.txt")
        .await
        .unwrap();

    // The mock only returns text for this exact URI, so the request carried
    // the URI in `params` as `resources/read` requires.
    assert_eq!(content.uri, "file:///tmp/test.txt");
    assert_eq!(content.mime_type, "text/plain");
    assert_eq!(content.text.as_deref(), Some("hello from resource"));
    assert!(content.blob.is_none());

    let cached = mgr
        .get_cached_resource("mock", "file:///tmp/test.txt")
        .expect("a successful read must be cached");
    assert_eq!(cached.text.as_deref(), Some("hello from resource"));

    mgr.stop_all().await;
}

#[tokio::test]
async fn resource_read_blob_is_returned_and_cached() {
    let mgr = running_mock_manager().await;

    let content = mgr
        .read_resource("mock", "file:///tmp/data.json")
        .await
        .unwrap();

    assert_eq!(content.uri, "file:///tmp/data.json");
    assert_eq!(content.mime_type, "application/json");
    assert_eq!(content.blob.as_deref(), Some("eyJvayI6dHJ1ZX0="));
    assert!(content.text.is_none());

    let cached = mgr
        .get_cached_resource("mock", "file:///tmp/data.json")
        .expect("a blob read must be cached");
    assert_eq!(cached.blob.as_deref(), Some("eyJvayI6dHJ1ZX0="));

    mgr.stop_all().await;
}

#[tokio::test]
async fn resource_read_any_finds_a_running_server() {
    let mgr = running_mock_manager().await;

    let content = mgr.read_resource_any("file:///tmp/test.txt").await.unwrap();
    assert_eq!(content.text.as_deref(), Some("hello from resource"));

    mgr.stop_all().await;
}

#[tokio::test]
async fn resource_read_surfaces_a_json_rpc_error() {
    let mgr = running_mock_manager().await;

    let error = mgr
        .read_resource("mock", "file:///tmp/boom")
        .await
        .unwrap_err();
    assert!(error.contains("resource unavailable"), "{error}");
    assert!(
        mgr.get_cached_resource("mock", "file:///tmp/boom")
            .is_none(),
        "a failed read must not be cached"
    );

    mgr.stop_all().await;
}

#[tokio::test]
async fn resource_read_empty_contents_is_an_error() {
    let mgr = running_mock_manager().await;

    let error = mgr
        .read_resource("mock", "file:///tmp/empty")
        .await
        .unwrap_err();
    assert!(error.contains("no contents"), "{error}");
    assert!(
        mgr.get_cached_resource("mock", "file:///tmp/empty")
            .is_none()
    );

    mgr.stop_all().await;
}

#[tokio::test]
async fn resource_read_entries_without_text_or_blob_are_an_error() {
    let mgr = running_mock_manager().await;

    let error = mgr
        .read_resource("mock", "file:///tmp/unsupported")
        .await
        .unwrap_err();
    assert!(error.contains("no readable contents"), "{error}");

    mgr.stop_all().await;
}

#[tokio::test]
async fn resource_read_unknown_server_is_an_error() {
    let mgr = McpManager::new();

    let error = mgr
        .read_resource("missing", "file:///tmp/x")
        .await
        .unwrap_err();
    assert!(error.contains("not attached"), "{error}");
}

#[tokio::test]
async fn resource_read_any_without_servers_is_an_error() {
    let mgr = McpManager::new();

    let error = mgr.read_resource_any("file:///tmp/x").await.unwrap_err();
    assert!(error.contains("no MCP server is attached"), "{error}");
}

#[tokio::test]
async fn resource_read_any_with_no_running_servers_is_an_error() {
    let mgr = McpManager::new();
    let mut config = McpConfig::new();
    config.add_server(
        "idle",
        McpServerConfig {
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
        },
    );
    mgr.load_config(&config);

    let error = mgr.read_resource_any("file:///tmp/x").await.unwrap_err();
    assert!(error.contains("no MCP server is running"), "{error}");
}

#[tokio::test]
async fn list_resource_info_exposes_discovered_metadata() {
    let mgr = running_mock_manager().await;

    let resources = mgr.list_resource_info();
    let resource = resources
        .iter()
        .find(|resource| resource.uri == "file:///tmp/test.txt")
        .expect("the discovered resource must be in the registry");
    assert_eq!(resource.name, "test.txt");
    assert_eq!(resource.mime_type, "text/plain");
    assert_eq!(resource.description.as_deref(), Some("A test file"));

    // The URI-only listing stays intact for existing callers.
    assert!(
        mgr.list_resources()
            .contains(&"file:///tmp/test.txt".to_string())
    );

    mgr.stop_all().await;
}

#[test]
fn resource_discovery_registry_filter_by_mime() {
    let mut reg = McpResourceRegistry::new();
    reg.register(McpResource {
        uri: "file:///a.txt".into(),
        name: "a".into(),
        mime_type: "text/plain".into(),
        description: None,
    });
    reg.register(McpResource {
        uri: "file:///b.json".into(),
        name: "b".into(),
        mime_type: "application/json".into(),
        description: None,
    });

    let text = reg.by_mime_prefix("text/");
    assert_eq!(text.len(), 1);
    assert_eq!(text[0].name, "a");

    let app = reg.by_mime_prefix("application/");
    assert_eq!(app.len(), 1);
    assert_eq!(app[0].name, "b");
}

// ═══════════════════════════════════════════════════════════════════════════
//  11.4 OAuth — token state, expiry, persistence
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn oauth_new_state_not_authenticated() {
    let state = McpOAuthState::new("http://localhost:8080", "client1");
    assert!(!state.is_authenticated());
    assert!(state.is_expired());
}

#[test]
fn oauth_update_tokens_authenticates() {
    let mut state = McpOAuthState::new("http://localhost:8080", "client1");
    state.update_tokens(
        "access_token_abc".into(),
        Some("refresh_token_xyz".into()),
        3600,
    );

    assert!(state.is_authenticated());
    assert!(!state.is_expired());
    assert!(!state.is_near_expiry());
    assert_eq!(state.access_token.as_ref().unwrap(), "access_token_abc");
    assert_eq!(state.refresh_token.as_ref().unwrap(), "refresh_token_xyz");
}

#[test]
fn oauth_expired_token_not_authenticated() {
    let mut state = McpOAuthState::new("http://localhost:8080", "client1");
    state.access_token = Some("token".into());
    state.expires_at = Some(0); // past

    assert!(!state.is_authenticated());
    assert!(state.is_expired());
}

#[test]
fn oauth_near_expiry_detects_within_5_minutes() {
    let mut state = McpOAuthState::new("http://localhost:8080", "client1");
    state.update_tokens("tok".into(), None, 300); // 5 min exactly

    assert!(state.is_near_expiry());
    assert!(!state.is_expired());
}

#[test]
fn oauth_manager_register_and_lookup() {
    let mut mgr = OAuthManager::new();
    let state = McpOAuthState::new("http://server1", "client1");
    mgr.register(state);

    assert_eq!(mgr.server_urls().len(), 1);
    assert!(!mgr.is_authenticated("http://server1"));
    assert!(mgr.get("http://server1").is_some());
    assert!(mgr.get("http://server2").is_none());
}

#[test]
fn oauth_manager_get_mut_updates_tokens() {
    let mut mgr = OAuthManager::new();
    let state = McpOAuthState::new("http://server1", "client1");
    mgr.register(state);

    if let Some(s) = mgr.get_mut("http://server1") {
        s.update_tokens("tok".into(), None, 3600);
    }

    assert!(mgr.is_authenticated("http://server1"));
}

#[test]
fn oauth_token_persistence_roundtrip() {
    // An explicit path is file-only: the test must never touch the developer's
    // keychain or ~/.coderex.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp_tokens.json");

    let mut mgr = OAuthManager::with_path(&path);
    let mut state = McpOAuthState::new("http://server1", "client1");
    state.update_tokens("access_123".into(), Some("refresh_456".into()), 3600);
    mgr.register(state);

    mgr.save_tokens_to(&path).unwrap();
    let loaded = OAuthManager::load_tokens_from(&path).unwrap();

    assert!(loaded.is_authenticated("http://server1"));
    let s = loaded.get("http://server1").unwrap();
    assert_eq!(s.access_token.as_ref().unwrap(), "access_123");
    assert_eq!(s.refresh_token.as_ref().unwrap(), "refresh_456");
}

/// The token the `McpAuth` tool writes has to be the token the MCP client
/// reads. The two live in different crates and cannot share a type — the
/// dependency runs `tools <- mcp` — so this is what keeps their idea of the
/// file from drifting apart. Before it, the tool wrote a bare JSON array that
/// `OAuthManager` could not parse, so a successful sign-in left the client
/// with no credential at all.
#[test]
fn a_token_written_by_the_mcp_auth_tool_is_readable_by_the_oauth_manager() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp_tokens.json");

    coderex_tools::persist_token_at(
        &path,
        "https://mcp.example",
        "coderex-cli",
        "tool_access",
        3600,
        Some("tool_refresh"),
    )
    .unwrap();

    let loaded = OAuthManager::load_tokens_from(&path).unwrap();
    assert!(loaded.is_authenticated("https://mcp.example"));
    let state = loaded.get("https://mcp.example").unwrap();
    assert_eq!(state.access_token.as_deref(), Some("tool_access"));
    assert_eq!(state.refresh_token.as_deref(), Some("tool_refresh"));
    assert_eq!(state.client_id, "coderex-cli");
    // The refresh path needs an endpoint; it derives the one the tool used.
    assert_eq!(state.token_endpoint(), "https://mcp.example/oauth/token");
}

/// The discovery cache has to be the same cache on both sides of the crate
/// boundary: `McpAuth` writes the endpoints, scopes and expiry, and
/// `OAuthManager::discover` reuses them instead of probing again. The issuer
/// here is a closed loopback port, so an attempted fetch would fail; returning
/// the cached document is what proves the two agree.
#[tokio::test]
async fn discovery_cached_by_the_mcp_auth_tool_is_reused_by_the_oauth_manager() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp_tokens.json");
    let issuer = "http://127.0.0.1:1/realms/demo";
    let authorization = "https://id.example/realms/demo/protocol/openid-connect/auth";
    let token = "https://id.example/realms/demo/protocol/openid-connect/token";

    coderex_tools::cache_discovery_at(
        &path,
        issuer,
        "coderex-cli",
        Some(authorization),
        Some(token),
        &["openid".to_string(), "mcp".to_string()],
        3600,
    )
    .unwrap();

    let mut mgr = OAuthManager::load_tokens_from(&path).unwrap();
    let discovery = mgr.discover(issuer, &reqwest::Client::new()).await.unwrap();
    assert_eq!(
        discovery.authorization_endpoint.as_deref(),
        Some(authorization)
    );
    assert_eq!(discovery.token_endpoint.as_deref(), Some(token));
    assert_eq!(discovery.scopes_supported, vec!["openid", "mcp"]);

    // The refresh path reads the same persisted endpoint.
    let state = mgr.get(issuer).unwrap();
    assert_eq!(state.token_endpoint(), token);
    assert_eq!(state.authorization_endpoint(), authorization);
}

/// The discovery cache written by `McpAuth` is keyed by the *normalized*
/// issuer, the same key `OAuthManager` uses. Two spellings of one issuer
/// (trailing slash, query) must not produce two cache entries.
#[test]
fn discovery_cached_by_the_tool_is_keyed_by_the_normalized_issuer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp_tokens.json");
    let spelled = "http://127.0.0.1:1/realms/demo/";
    let issuer = "http://127.0.0.1:1/realms/demo";
    let token = "https://id.example/realms/demo/protocol/openid-connect/token";

    coderex_tools::cache_discovery_at(
        &path,
        spelled,
        "coderex-cli",
        Some("https://id.example/realms/demo/protocol/openid-connect/auth"),
        Some(token),
        &["openid".to_string()],
        3600,
    )
    .unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["states"][0]["issuer"], issuer);
    assert!(
        raw["issuer_metadata"].get(issuer).is_some(),
        "the shared cache is not keyed by the normalized issuer: {raw}"
    );
    assert!(raw["issuer_metadata"].get(spelled).is_none());

    let loaded = OAuthManager::load_tokens_from(&path).unwrap();
    let state = loaded.get(spelled).unwrap();
    assert_eq!(state.issuer.as_deref(), Some(issuer));
    assert_eq!(state.token_endpoint(), token);
}

/// The RFC 9728 / RFC 7591 fields the `McpAuth` tool writes have to survive
/// its own read-modify-write and be readable by `OAuthManager`. The crates
/// cannot share the type (`tools <- mcp`), so the on-disk shape is the
/// contract; this pins both sides of it.
#[test]
fn the_discovery_fields_written_by_the_tool_survive_and_parse_in_the_manager() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp_tokens.json");
    let server = "https://mcp.example/mcp";
    let issuer = "https://id.example/realms/demo";
    let document = serde_json::json!({
        "states": [{
            "server_url": server,
            "client_id": "dyn-client",
            "client_secret": "dyn-secret",
            "access_token": "old",
            "refresh_token": "refresh",
            "expires_at": 1,
            "scopes": [],
            "scopes_supported": ["read"],
            "discovery_expires_at": 4_000_000_000_i64,
            "token_endpoint": "https://id.example/realms/demo/token",
            "authorization_endpoint": "https://id.example/realms/demo/auth",
            "issuer": issuer,
            "resource": server,
            "protected_resource": {
                "resource": server,
                "authorization_servers": [issuer],
                "scopes_supported": ["read"]
            },
            "resource_metadata_url": "https://mcp.example/.well-known/oauth-protected-resource/mcp",
            "resource_metadata_expires_at": 4_000_000_000_i64,
            "resource_metadata_etag": "\"v1\"",
            "registration_endpoint": "https://id.example/realms/demo/register",
            "token_endpoint_auth_method": "client_secret_basic",
            "token_endpoint_auth_methods_supported": ["client_secret_basic"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"]
        }],
        "issuer_metadata": {
            issuer: {
                "issuer": issuer,
                "token_endpoint": "https://id.example/realms/demo/token",
                "authorization_endpoint": "https://id.example/realms/demo/auth",
                "registration_endpoint": "https://id.example/realms/demo/register",
                "scopes_supported": ["read"],
                "token_endpoint_auth_methods_supported": ["client_secret_basic"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "expires_at": 4_000_000_000_i64,
                "etag": "\"v1\"",
                "source_url": "https://id.example/.well-known/oauth-authorization-server/realms/demo"
            }
        }
    });
    std::fs::write(&path, document.to_string()).unwrap();

    // The tool's read-modify-write path must not drop fields it does not set.
    coderex_tools::persist_token_at(&path, server, "dyn-client", "new", 3600, Some("refresh"))
        .unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        raw["issuer_metadata"].get(issuer).is_some(),
        "the tool dropped the issuer cache: {raw}"
    );
    assert_eq!(raw["states"][0]["client_secret"], "dyn-secret");
    assert_eq!(raw["states"][0]["resource_metadata_etag"], "\"v1\"");
    assert_eq!(
        raw["states"][0]["registration_endpoint"],
        "https://id.example/realms/demo/register"
    );

    let loaded = OAuthManager::load_tokens_from(&path).unwrap();
    let state = loaded.get(server).unwrap();
    assert_eq!(state.client_secret.as_deref(), Some("dyn-secret"));
    assert_eq!(state.issuer.as_deref(), Some(issuer));
    assert_eq!(state.resource(), server);
    assert_eq!(state.resource_metadata_etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        state.token_endpoint_auth_method.as_deref(),
        Some("client_secret_basic")
    );
    assert_eq!(
        state.registration_endpoint.as_deref(),
        Some("https://id.example/realms/demo/register")
    );
    assert_eq!(
        state.grant_types_supported,
        vec!["authorization_code", "refresh_token"]
    );
    assert_eq!(state.code_challenge_methods_supported, vec!["S256"]);
    assert_eq!(
        state
            .protected_resource
            .as_ref()
            .and_then(|metadata| metadata.resource.as_deref()),
        Some(server)
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  11.5 Connection Management — reconnect after failure
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn connection_reconnect_restores_channel() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());
    server.max_reconnects = 3;

    let result: Result<(), _> = server.spawn().await;
    result.unwrap();
    assert_eq!(server.status, ServerStatus::Running);

    // Simulate a reconnection
    let result: Result<(), _> = server.reconnect().await;
    result.unwrap();
    assert_eq!(server.status, ServerStatus::Running);
    assert!(server.channel().is_some());
    assert_eq!(server.reconnect_count, 1);

    server.stop().await;
}

#[tokio::test]
async fn connection_reconnect_exhausts_max_attempts() {
    let script = mock_server_path();
    let mut server = McpServer::new("test", script.to_str().unwrap(), vec![], HashMap::new());
    server.max_reconnects = 2;

    server.spawn().await.unwrap();

    let result: Result<(), _> = server.reconnect().await;
    result.unwrap(); // count = 1
    let result: Result<(), _> = server.reconnect().await;
    result.unwrap(); // count = 2

    let result: Result<(), _> = server.reconnect().await;
    assert!(result.is_err());
    assert_eq!(server.status, ServerStatus::Errored);
}

#[tokio::test]
async fn connection_manager_stop_all_shuts_down_servers() {
    let script = mock_server_path();
    let mgr = McpManager::new();
    let mut config = McpConfig::new();
    config.add_server(
        "mock1",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
        },
    );
    config.add_server(
        "mock2",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
        },
    );
    mgr.load_config(&config);

    mgr.start_all().await;
    assert!(mgr.is_running("mock1"));
    assert!(mgr.is_running("mock2"));

    mgr.stop_all().await;
    assert!(!mgr.is_running("mock1"));
    assert!(!mgr.is_running("mock2"));
}

#[tokio::test]
async fn connection_channel_spawn_and_terminate() {
    let script = mock_server_path();
    let mut channel = JsonRpcChannel::new(script.to_str().unwrap(), vec![], HashMap::new());

    assert!(!channel.is_running());

    channel.spawn().await.unwrap();
    assert!(channel.is_running());

    channel.terminate().await.unwrap();
    assert!(!channel.is_running());
}

#[tokio::test]
async fn connection_channel_initialize_handshake() {
    let script = mock_server_path();
    let mut channel = JsonRpcChannel::new(script.to_str().unwrap(), vec![], HashMap::new());

    channel.spawn().await.unwrap();
    let result = channel.initialize().await;
    assert!(result.is_ok(), "initialize failed: {result:?}");

    let value = result.unwrap();
    assert!(value.get("protocolVersion").is_some());
    assert!(channel.is_initialized());

    let _ = channel.terminate().await;
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: Config save/load, result adapter
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn config_save_load_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".mcp.json");

    let mut config = McpConfig::new();
    config.add_server(
        "filesystem",
        McpServerConfig {
            command: "npx".into(),
            args: vec![
                "@modelcontextprotocol/server-filesystem".into(),
                "/tmp".into(),
            ],
            env: HashMap::new(),
            ..Default::default()
        },
    );

    config.save(&path).unwrap();
    let loaded = McpConfig::load(&path).unwrap();

    assert!(!loaded.is_empty());
    let srv = loaded.get_server("filesystem").unwrap();
    assert_eq!(srv.command, "npx");
    assert_eq!(srv.args.len(), 2);
}

#[test]
fn result_adapter_text_content() {
    let result = serde_json::json!({
        "content": [{"type": "text", "text": "hello from mcp"}],
        "isError": false
    });
    let tr = McpResultAdapter::adapt("call-1", &result);
    assert!(!tr.is_error);
    assert_eq!(tr.output, "hello from mcp");
}

#[test]
fn result_adapter_error_result() {
    let result = serde_json::json!({
        "content": [{"type": "text", "text": "something went wrong"}],
        "isError": true
    });
    let tr = McpResultAdapter::adapt("call-1", &result);
    assert!(tr.is_error);
    assert_eq!(tr.output, "something went wrong");
}

#[test]
fn result_adapter_multiple_content_blocks() {
    let result = serde_json::json!({
        "content": [
            {"type": "text", "text": "first block"},
            {"type": "text", "text": "second block"}
        ],
        "isError": false
    });
    let tr = McpResultAdapter::adapt("call-1", &result);
    assert_eq!(tr.output, "first block\n\nsecond block");
}
