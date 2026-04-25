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
use coderex_mcp::tool_executor::McpToolExecutor;

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
    let path = tmp.join(format!("coderex_mock_mcp_server_{id}.py"));
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
        },
    );
    mgr.load_config(&config);

    mgr.start_all().await;

    assert!(mgr.is_running("mock"));
    assert_eq!(mgr.tool_count(), 2);
    let tools = mgr.list_tools();
    assert!(tools.contains(&"mock_read_file".to_string()));
    assert!(tools.contains(&"mock_write_file".to_string()));

    mgr.stop_all().await;
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
async fn tool_discovery_executor_routes_to_mcp() {
    let script = mock_server_path();
    let mgr = McpManager::new();
    let mut config = McpConfig::new();
    config.add_server(
        "mock",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
        },
    );
    mgr.load_config(&config);
    mgr.start_all().await;

    let executor = McpToolExecutor::new(std::sync::Arc::new(mgr));
    let outcome = executor
        .execute(
            "tc_1",
            "mock_read_file",
            &serde_json::json!({"path": "/tmp/test.txt"}),
        )
        .await;

    assert!(!outcome.result.is_error);
    assert!(outcome.result.output.contains("Executed mock_read_file"));
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
    let mut mgr = OAuthManager::new();
    let mut state = McpOAuthState::new("http://server1", "client1");
    state.update_tokens("access_123".into(), Some("refresh_456".into()), 3600);
    mgr.register(state);

    mgr.save_tokens().unwrap();
    let loaded = OAuthManager::load_tokens().unwrap();

    assert!(loaded.is_authenticated("http://server1"));
    let s = loaded.get("http://server1").unwrap();
    assert_eq!(s.access_token.as_ref().unwrap(), "access_123");
    assert_eq!(s.refresh_token.as_ref().unwrap(), "refresh_456");

    // Clean up
    let _ = std::fs::remove_file(OAuthManager::token_path());
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
        },
    );
    config.add_server(
        "mock2",
        McpServerConfig {
            command: script.to_str().unwrap().into(),
            args: vec![],
            env: HashMap::new(),
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
