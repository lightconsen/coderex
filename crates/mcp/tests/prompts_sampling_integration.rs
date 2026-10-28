//! Integration tests for MCP `prompts` and `sampling`.
//!
//! A Python stdio server advertises both capabilities, serves `prompts/list`
//! and `prompts/get`, and asks the client to sample. The tests drive it through
//! `McpServer` and `McpManager`, so the transport, parsing and routing are all
//! exercised rather than the parsers alone.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coderex_mcp::channel::METHOD_NOT_FOUND;
use coderex_mcp::config::{McpConfig, McpServerConfig};
use coderex_mcp::elicitation::{
    McpRequestRouter, SamplingContent, SamplingHandler, SamplingRequest, SamplingResult,
    SamplingRole,
};
use coderex_mcp::manager::McpManager;
use coderex_mcp::server::McpServer;

static MOCK_ID: AtomicU64 = AtomicU64::new(0);

const MOCK_SERVER: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

pending_tool = None
prompt_lists = 0

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
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        continue

    # A response from the client (the answer to our sampling request).
    if "method" not in req:
        if req.get("id") == 900 and pending_tool is not None:
            if "result" in req:
                text = req["result"].get("content", {}).get("text", "")
                respond(pending_tool, {
                    "content": [{"type": "text", "text": "sampled:" + text}],
                    "isError": False
                })
            else:
                message = req.get("error", {}).get("message", "no answer")
                respond(pending_tool, {
                    "content": [{"type": "text", "text": "refused:" + message}],
                    "isError": True
                })
            pending_tool = None
        continue

    method = req.get("method", "")
    rid = req.get("id")
    params = req.get("params") or {}

    # MOCK_METHOD_LOG lets a test prove a request was never sent.
    log_path = os.environ.get("MOCK_METHOD_LOG")
    if log_path and method:
        with open(log_path, "a") as log:
            log.write(method + "\n")

    if method == "initialize":
        # MOCK_NO_PROMPTS advertises only tools, the shape of a server that
        # has no prompts at all.
        capabilities = {"tools": {}}
        if not os.environ.get("MOCK_NO_PROMPTS"):
            capabilities["prompts"] = {}
        respond(rid, {
            "protocolVersion": "2024-11-05",
            "capabilities": capabilities,
            "serverInfo": {"name": "mock-prompts", "version": "1.0"}
        })
    elif method in ("initialized", "notifications/initialized"):
        pass
    elif method == "tools/list":
        respond(rid, {"tools": [
            {
                "name": "ask",
                "description": "Ask the client's model",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "shrink",
                "description": "Drop a prompt and notify",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]})
    elif method == "prompts/list":
        prompt_lists += 1
        prompts = [
            {
                "name": "greet",
                "description": "Greet someone",
                "arguments": [{"name": "name", "required": True}]
            },
            {"name": "review", "description": "Review code"}
        ]
        if prompt_lists > 1:
            prompts = prompts[1:]
        respond(rid, {"prompts": prompts})
    elif method == "prompts/get":
        if params.get("name") == "greet":
            arguments = params.get("arguments") or {}
            respond(rid, {
                "description": "Greet someone",
                "messages": [{
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": "Hello " + str(arguments.get("name", "?"))
                    }
                }]
            })
        else:
            error(rid, -32602, "unknown prompt")
    elif method == "tools/call":
        if params.get("name") == "ask":
            pending_tool = rid
            send({
                "jsonrpc": "2.0",
                "id": 900,
                "method": "sampling/createMessage",
                "params": {
                    "messages": [{
                        "role": "user",
                        "content": {"type": "text", "text": "What is 2+2?"}
                    }],
                    "maxTokens": 32
                }
            })
        elif params.get("name") == "shrink":
            respond(rid, {"content": [{"type": "text", "text": "ok"}], "isError": False})
            send({"jsonrpc": "2.0", "method": "notifications/prompts/list_changed"})
        else:
            error(rid, -32602, "unknown tool")
    else:
        error(rid, -32601, "Method " + method + " not found")
"#;

fn mock_server_path() -> std::path::PathBuf {
    let id = MOCK_ID.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "coderex_prompt_sample_server_{}_{id}.py",
        std::process::id()
    ));
    std::fs::write(&path, MOCK_SERVER).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

fn server_config() -> McpServerConfig {
    McpServerConfig {
        command: mock_server_path().to_str().unwrap().into(),
        args: vec![],
        env: HashMap::new(),
        ..Default::default()
    }
}

async fn spawn_server() -> McpServer {
    let config = server_config();
    let mut server = McpServer::new("mock", config.command.as_str(), vec![], HashMap::new());
    server.spawn().await.unwrap();
    server
}

async fn running_manager() -> McpManager {
    let manager = McpManager::new();
    let mut config = McpConfig::new();
    config.add_server("mock", server_config());
    manager.load_config(&config);
    manager.start_all().await;
    manager
}

#[derive(Debug, Default)]
struct FixedSampling {
    requests: Mutex<Vec<SamplingRequest>>,
}

#[async_trait::async_trait]
impl SamplingHandler for FixedSampling {
    async fn sample(
        &self,
        request: &SamplingRequest,
    ) -> Result<SamplingResult, coderex_mcp::channel::JsonRpcError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(SamplingResult {
            role: SamplingRole::Assistant,
            content: SamplingContent::text("fixed answer"),
            model: "test-model".into(),
        })
    }
}

/// A server that does not advertise `prompts` is not asked for them.
///
/// The discovery path returns "no prompts" without a `prompts/list` request:
/// a server that never announced the capability may never answer, so probing
/// it would leave discovery waiting on a reply that is not coming. The method
/// log is what proves nothing was sent.
#[tokio::test]
async fn a_server_without_the_prompts_capability_is_not_asked() {
    let log = std::env::temp_dir().join(format!(
        "coderex_prompt_methods_{}_{}.log",
        std::process::id(),
        MOCK_ID.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_file(&log);
    let mut env = HashMap::new();
    env.insert("MOCK_NO_PROMPTS".to_string(), "1".to_string());
    env.insert(
        "MOCK_METHOD_LOG".to_string(),
        log.to_string_lossy().into_owned(),
    );
    let config = McpServerConfig {
        command: mock_server_path().to_str().unwrap().into(),
        args: vec![],
        env,
        ..Default::default()
    };
    let mut server = McpServer::new("mock", config.command.as_str(), vec![], config.env.clone());
    server.spawn().await.unwrap();

    assert!(!server.supports_prompts(), "the mock advertised prompts");
    let discovered = server.discover_prompts().await.expect("discovery");
    assert_eq!(
        discovered,
        serde_json::json!({"prompts": []}),
        "a server without the capability should report no prompts"
    );

    let methods = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    assert!(
        !methods.lines().any(|line| line == "prompts/list"),
        "prompts/list was sent to a server that never advertised it: {methods}"
    );
}

#[tokio::test]
async fn prompts_are_discovered_and_resolved_over_stdio() {
    let mut server = spawn_server().await;
    assert!(server.supports_prompts());

    let list = server.discover_prompts().await.expect("prompts/list");
    let prompts = coderex_mcp::prompts::parse_prompt_list(&list).expect("parse");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].name, "greet");
    assert_eq!(
        prompts[0].arguments[0].name, "name",
        "{:?}",
        prompts[0].arguments
    );

    let mut arguments = serde_json::Map::new();
    arguments.insert("name".into(), serde_json::Value::String("Ada".into()));
    let result = server
        .get_prompt("greet", &arguments)
        .await
        .expect("prompts/get");
    let parsed = coderex_mcp::prompts::parse_prompt_result(&result).expect("parse");
    assert_eq!(parsed.description.as_deref(), Some("Greet someone"));
    assert_eq!(parsed.messages.len(), 1);
    assert_eq!(
        parsed.messages[0].content.render_text(),
        "Hello Ada",
        "{:?}",
        parsed.messages
    );

    server.stop().await;
}

#[tokio::test]
async fn the_manager_registers_prompts_and_resolves_them_by_server() {
    let manager = running_manager().await;
    assert_eq!(manager.prompt_count(), 2);
    assert!(manager.prompt_info("mock", "greet").is_some());
    assert_eq!(manager.prompts_for_server("mock").len(), 2);

    let mut arguments = serde_json::Map::new();
    arguments.insert("name".into(), serde_json::Value::String("Grace".into()));
    let result = manager
        .get_prompt("mock", "greet", arguments)
        .await
        .expect("prompt resolves");
    assert_eq!(result.messages[0].content.render_text(), "Hello Grace");

    let error = manager
        .get_prompt("mock", "missing", serde_json::Map::new())
        .await
        .expect_err("unknown prompt is an error");
    assert!(error.contains("missing"), "{error}");

    manager.stop_all().await;
}

#[tokio::test]
async fn a_list_changed_notification_drops_prompts_the_server_no_longer_offers() {
    let manager = running_manager().await;
    assert_eq!(manager.prompt_count(), 2);

    let server = manager.get_server("mock").expect("server is running");
    let channel = server.channel().expect("channel");
    let response = channel
        .lock()
        .await
        .send_request(
            "tools/call",
            Some(serde_json::json!({"name": "shrink", "arguments": {}})),
        )
        .await
        .expect("shrink runs");
    assert_eq!(response["content"][0]["text"], "ok");

    // The notification is written after the response; a stdio channel reads
    // it on the next request, so give it one. Its handler is dispatched on a
    // task, which is what lets the re-discovery take the channel again.
    let _: serde_json::Value = channel
        .lock()
        .await
        .send_request("tools/list", None)
        .await
        .expect("the next request reads the queued notification");

    let deadline = Instant::now() + Duration::from_secs(5);
    while manager.prompt_count() != 1 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(manager.prompt_count(), 1, "the stale prompt was dropped");
    assert!(manager.prompt_info("mock", "greet").is_none());
    assert!(manager.prompt_info("mock", "review").is_some());

    manager.stop_all().await;
}

#[tokio::test]
async fn sampling_requests_reach_the_installed_handler() {
    let handler = Arc::new(FixedSampling::default());
    let router = Arc::new(McpRequestRouter::new(Some(handler.clone()), None));
    let config = server_config();
    let mut server = McpServer::new("mock", config.command.as_str(), vec![], HashMap::new());
    server.set_inbound_handler(router);
    server.spawn().await.unwrap();

    let response = server
        .channel()
        .expect("channel")
        .lock()
        .await
        .send_request(
            "tools/call",
            Some(serde_json::json!({"name": "ask", "arguments": {}})),
        )
        .await
        .expect("ask runs");

    assert_eq!(response["content"][0]["text"], "sampled:fixed answer");
    {
        let requests = handler.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].max_tokens, 32);
        assert_eq!(
            requests[0].messages[0].content.as_text(),
            Some("What is 2+2?")
        );
    }

    server.stop().await;
}

#[tokio::test]
async fn sampling_without_a_handler_is_refused_with_the_opt_in() {
    // The default router has no sampling handler: the refusal must name the
    // opt-in so a server author can tell it from an unimplemented method.
    let mut server = McpServer::new(
        "mock",
        server_config().command.as_str(),
        vec![],
        HashMap::new(),
    );
    server.set_inbound_handler(Arc::new(McpRequestRouter::new(None, None)));
    server.spawn().await.expect("spawn");

    let response = server
        .channel()
        .expect("channel")
        .lock()
        .await
        .send_request(
            "tools/call",
            Some(serde_json::json!({"name": "ask", "arguments": {}})),
        )
        .await
        .expect("ask runs");

    let text = response["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("refused:"), "{text}");
    assert!(
        text.contains("CODEREX_MCP_SAMPLING"),
        "the refusal names the opt-in: {text}"
    );
    assert!(response["isError"].as_bool().unwrap_or(false));

    server.stop().await;
}

#[test]
fn an_unhandled_inbound_request_gets_method_not_found() {
    // The default request router has neither a sampling handler nor a
    // fallback; it must say so rather than leave the server waiting.
    let router = McpRequestRouter::new(None, None);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let handler: &dyn coderex_mcp::channel::InboundRequestHandler = &router;
    let error = runtime
        .block_on(handler.handle(
            "sampling/createMessage",
            Some(&serde_json::json!({
                "messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}],
                "maxTokens": 8
            })),
        ))
        .expect_err("sampling is disabled");
    assert_eq!(error.code, METHOD_NOT_FOUND);
    assert!(
        error.message.contains("CODEREX_MCP_SAMPLING"),
        "{}",
        error.message
    );
}
