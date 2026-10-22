//! Hook integration tests — covering all 22 hook events, subprocess execution,
//! result parsing, and lifecycle behavior.
//!
//! These tests correspond to the equivalence test plan Module 5:
//! 5.1 SessionStart, 5.2 UserPromptSubmit, 5.3 PreToolUse, 5.4 PostToolUse,
//! 5.5 PostToolUseFailure, 5.6 Stop, 5.7 PreCompact, 5.8 PostCompact,
//! 5.9 CwdChanged

use std::collections::HashMap;

use coderex_core::hook_executor::HookExecutor;
use coderex_hooks::events::HookEvent;
use coderex_hooks::{
    HookAction, HookConfig, HookInput, HookRegistry, HookRequestError, HookRequestHandler,
    HookStreamEvent, HookTier, HookType,
};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn config(event: &str, command: &str) -> HookConfig {
    HookConfig {
        watch: None,
        event: event.to_string(),
        kind: HookType::Command,
        command: command.to_string(),
        http: None,
        prompt: None,
        model: None,
        mcp: None,
        env: HashMap::new(),
        cwd: None,
        internal: false,
        timeout_ms: 5000,
        priority: 0,
        dry_run: false,
        stream: false,
        max_cpu_seconds: None,
        max_memory_mb: None,
        max_file_size_mb: None,
        sandbox: None,
        tier: HookTier::Core,
    }
}

fn config_with_env(event: &str, command: &str, env: HashMap<String, String>) -> HookConfig {
    HookConfig {
        watch: None,
        event: event.to_string(),
        kind: HookType::Command,
        command: command.to_string(),
        http: None,
        prompt: None,
        model: None,
        mcp: None,
        env,
        cwd: None,
        internal: false,
        timeout_ms: 5000,
        priority: 0,
        dry_run: false,
        stream: false,
        max_cpu_seconds: None,
        max_memory_mb: None,
        max_file_size_mb: None,
        sandbox: None,
        tier: HookTier::Core,
    }
}

fn input(event: &str) -> HookInput {
    HookInput {
        event: event.to_string(),
        session_id: Some("test-session".to_string()),
        input: Some("test input".to_string()),
        context: None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.1 SessionStart — the registry runs a configured hook
//
// SessionStart is a reserved name: the engine never fires it. These tests
// exercise `execute_sync` on the registry, not a firing site.
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_sessionstart_runs_through_the_registry() {
    let mut registry = HookRegistry::new();
    registry.register(config("SessionStart", r#"echo '{"action":"continue"}'"#));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert_eq!(results.len(), 1);

    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn hook_sessionstart_receives_input_on_stdin() {
    let mut registry = HookRegistry::new();
    // Read stdin and echo back the first bytes we received, as JSON, using
    // python3 or python. `json.dumps` escapes the input so the echoed value is
    // still valid JSON.
    registry.register(config(
        "SessionStart",
        r#"python3 -c "import sys,json; print(json.dumps({'action':'continue','additional_context':sys.stdin.read()[:20]}))" 2>/dev/null || python -c "import sys,json; print(json.dumps({'action':'continue','additional_context':sys.stdin.read()[:20]}))""#,
    ));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    // The hook should have received the JSON input on stdin (starts with {"event":)
    let echoed = result.hook_result.additional_context.clone().unwrap();
    assert!(
        echoed.contains("event"),
        "hook did not receive its input on stdin: {echoed:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.2 UserPromptSubmit — hook can inspect and modify user input
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_userpromptsubmit_can_modify_input() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "UserPromptSubmit",
        r#"echo '{"action":"continue", "updated_input": "modified prompt"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::UserPromptSubmit, &input("UserPromptSubmit"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert_eq!(
        result.hook_result.updated_input,
        Some("modified prompt".to_string())
    );
}

#[tokio::test]
async fn hook_userpromptsubmit_stop_prevents_processing() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "UserPromptSubmit",
        r#"echo '{"action":"stop", "suppress_output": true}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::UserPromptSubmit, &input("UserPromptSubmit"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Stop));
    assert!(result.hook_result.suppress_output);
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.3 PreToolUse — hook fires before tool execution, can stop it
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_pretooluse_can_stop_tool_execution() {
    let mut registry = HookRegistry::new();
    registry.register(config("PreToolUse", r#"echo '{"action":"stop"}'"#));

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Stop));
}

#[tokio::test]
async fn hook_pretooluse_continue_allows_tool_execution() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PreToolUse",
        r#"echo '{"action":"continue", "watch_paths": ["/src", "/tests"]}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(result.hook_result.watch_paths, vec!["/src", "/tests"]);
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.4 PostToolUse — hook fires after successful tool execution
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_posttooluse_fires_after_success() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PostToolUse",
        r#"echo '{"action":"continue", "additional_context": "tool succeeded"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PostToolUse, &input("PostToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(
        result.hook_result.additional_context,
        Some("tool succeeded".to_string())
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.5 PostToolUseFailure — hook fires after failed tool execution
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_posttoolusefailure_fires_after_failure() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PostToolUseFailure",
        r#"echo '{"action":"continue", "additional_context": "tool failed, retrying"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PostToolUseFailure, &input("PostToolUseFailure"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(
        result.hook_result.additional_context,
        Some("tool failed, retrying".to_string())
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.6 Stop — hook fires when user requests stop
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_stop_fires_on_stop_request() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "Stop",
        r#"echo '{"action":"continue", "suppress_output": true}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::Stop, &input("Stop"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert!(result.hook_result.suppress_output);
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.7 PreCompact — hook fires before context compaction
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_precompact_fires_before_compaction() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PreCompact",
        r#"echo '{"action":"continue", "additional_context": "compact starting"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PreCompact, &input("PreCompact"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.8 PostCompact — hook fires after context compaction
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_postcompact_fires_after_compaction() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PostCompact",
        r#"echo '{"action":"continue", "additional_context": "compact complete"}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PostCompact, &input("PostCompact"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}

// ═══════════════════════════════════════════════════════════════════════════
//  5.9 CwdChanged — the registry runs a configured hook
//
// CwdChanged is fired by `RegistryToolDispatcher::set_cwd` when a session is
// moved (e.g. into a worktree); this test exercises `execute_sync` on the
// registry, not the firing site.
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_cwdchanged_runs_through_the_registry() {
    // `watch_paths` must be absolute: a relative path would be resolved
    // against whatever cwd the watcher happens to have.
    let watched = std::env::temp_dir()
        .join(".coderex")
        .to_string_lossy()
        .into_owned();
    let answer = serde_json::json!({
        "action": "continue",
        "watch_paths": [watched.clone()],
    })
    .to_string();
    let mut registry = HookRegistry::new();
    registry.register(config("CwdChanged", &format!("echo '{answer}'")));

    let results = registry
        .execute_sync(&HookEvent::CwdChanged, &input("CwdChanged"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));
    assert_eq!(result.hook_result.watch_paths, vec![watched]);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: matching, priority, async, timeout, settings
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn hook_wildcard_matches_all_events() {
    let mut registry = HookRegistry::new();
    registry.register(config("*", r#"echo '{"action":"continue"}'"#));

    for event in [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::Stop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::CwdChanged,
    ] {
        let results = registry.execute_sync(&event, &input("test")).await;
        assert!(!results.is_empty(), "wildcard hook should match {event:?}");
    }
}

#[tokio::test]
async fn hook_priority_runs_highest_first() {
    let mut registry = HookRegistry::new();
    registry.register(HookConfig {
        watch: None,
        priority: 1,
        ..config(
            "PreToolUse",
            r#"echo '{"action":"continue", "updated_input": "high"}'"#,
        )
    });
    registry.register(HookConfig {
        watch: None,
        priority: 0,
        ..config(
            "PreToolUse",
            r#"echo '{"action":"continue", "updated_input": "low"}'"#,
        )
    });

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    assert_eq!(results.len(), 2);
    // Highest priority runs first
    assert_eq!(
        results[0].as_ref().unwrap().hook_result.updated_input,
        Some("high".to_string())
    );
    assert_eq!(
        results[1].as_ref().unwrap().hook_result.updated_input,
        Some("low".to_string())
    );
}

#[tokio::test]
async fn hook_timeout_kills_long_running_hook() {
    let mut registry = HookRegistry::new();
    registry.register(HookConfig {
        watch: None,
        timeout_ms: 100,
        ..config("Stop", "sleep 10")
    });

    let results = registry
        .execute_sync(&HookEvent::Stop, &input("Stop"))
        .await;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err(), "long-running hook should time out");
}

#[tokio::test]
async fn hook_async_execution_runs_concurrently() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "SessionStart",
        r#"echo '{"action":"continue", "additional_context": "async1"}'"#,
    ));
    registry.register(config(
        "SessionStart",
        r#"echo '{"action":"continue", "additional_context": "async2"}'"#,
    ));

    let results = registry
        .execute_async(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert_eq!(results.len(), 2);
    let contexts: Vec<_> = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter_map(|r| r.hook_result.additional_context.clone())
        .collect();
    assert!(contexts.contains(&"async1".to_string()));
    assert!(contexts.contains(&"async2".to_string()));
}

#[tokio::test]
async fn hook_fire_hook_trait_impl_combines_results() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "UserPromptSubmit",
        r#"echo '{"action":"continue", "updated_input": "first"}'"#,
    ));
    registry.register(config("UserPromptSubmit", r#"echo '{"action":"stop"}'"#));

    let mut context = HashMap::new();
    context.insert("session_id".to_string(), "sess_123".to_string());

    let result = registry
        .fire_hook("UserPromptSubmit", "original", &context)
        .await;

    // stop should be true because one hook returned stop
    assert!(result.stop);
    // updated_input should come from the first hook (last one wins in the impl)
    assert_eq!(result.updated_input, Some("first".to_string()));
}

#[test]
fn hook_load_from_settings_parses_hooks_array() {
    let value = serde_json::json!({
        "hooks": [
            {
                "event": "SessionStart",
                "command": "echo start",
                "env": {"FOO": "bar"},
                "timeout_ms": 3000,
                "priority": 5
            },
            {
                "event": "PreToolUse",
                "command": "echo pre",
                "dry_run": true
            }
        ]
    });

    let registry = HookRegistry::load_from_settings(&value).unwrap();
    let session_start = registry.matches_for_event(&HookEvent::SessionStart);
    assert_eq!(session_start.len(), 1);
    assert_eq!(session_start[0].command, "echo start");
    assert_eq!(session_start[0].env.get("FOO").unwrap(), "bar");
    assert_eq!(session_start[0].timeout_ms, 3000);
    assert_eq!(session_start[0].priority, 5);

    let pre_tool = registry.matches_for_event(&HookEvent::PreToolUse);
    assert_eq!(pre_tool.len(), 1);
    assert!(pre_tool[0].dry_run);
}

#[test]
fn hook_load_from_settings_null_returns_empty() {
    let registry = HookRegistry::load_from_settings(&serde_json::Value::Null).unwrap();
    assert!(
        registry
            .matches_for_event(&HookEvent::SessionStart)
            .is_empty()
    );
}

#[tokio::test]
async fn hook_dry_run_suppresses_stop_action() {
    let mut registry = HookRegistry::new();
    registry.register(HookConfig {
        watch: None,
        dry_run: true,
        stream: false,
        ..config("PreToolUse", r#"echo '{"action":"stop"}'"#)
    });

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    // In dry-run mode, stop is converted to continue
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}

#[tokio::test]
async fn hook_env_variables_are_injected() {
    let mut env = HashMap::new();
    env.insert("HOOK_CUSTOM_VAR".to_string(), "custom_value".to_string());

    let mut registry = HookRegistry::new();
    registry.register(config_with_env(
        "SessionStart",
        r#"echo '{"action":"continue", "additional_context": "'$HOOK_CUSTOM_VAR'"}'"#,
        env,
    ));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert_eq!(
        result.hook_result.additional_context,
        Some("custom_value".to_string())
    );
}

#[tokio::test]
async fn hook_no_match_returns_empty_results() {
    let registry = HookRegistry::new();
    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert!(results.is_empty(), "no hooks registered = no results");
}

#[tokio::test]
async fn hook_permission_decision_parsed_from_output() {
    let mut registry = HookRegistry::new();
    registry.register(config(
        "PermissionRequest",
        r#"echo '{"action":"continue", "permission_decision": {"allow": true, "allow_for_path": "/tmp", "reason": "safe path"}}'"#,
    ));

    let results = registry
        .execute_sync(&HookEvent::PermissionRequest, &input("PermissionRequest"))
        .await;
    let result = results[0].as_ref().unwrap();
    let decision = result.hook_result.permission_decision.as_ref().unwrap();
    assert!(decision.allow);
    assert_eq!(decision.allow_for_path, Some("/tmp".to_string()));
    assert_eq!(decision.reason, "safe path");
}

#[tokio::test]
async fn hook_nonzero_exit_is_a_failure() {
    let mut registry = HookRegistry::new();
    registry.register(config("SessionStart", "exit 1"));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    // A hook that exits non-zero did not answer; it is not a silent Continue.
    assert!(matches!(
        results[0],
        Err(coderex_hooks::HookError::ExecutionFailed(1))
    ));
}

#[tokio::test]
async fn hook_empty_stdout_is_a_failure() {
    let mut registry = HookRegistry::new();
    // `true` exits 0 and prints nothing — a hook that gave no answer.
    registry.register(config("SessionStart", "true"));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert!(matches!(
        results[0],
        Err(coderex_hooks::HookError::NoOutput)
    ));
}

#[tokio::test]
async fn hook_unparseable_stdout_is_a_failure() {
    let mut registry = HookRegistry::new();
    registry.register(config("SessionStart", "echo not-json"));

    let results = registry
        .execute_sync(&HookEvent::SessionStart, &input("SessionStart"))
        .await;
    assert!(matches!(
        results[0],
        Err(coderex_hooks::HookError::UnparseableOutput(_))
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  http hooks — the input is POSTed, the response body is the answer
// ═══════════════════════════════════════════════════════════════════════════

/// A one-request HTTP endpoint.
///
/// Serves exactly one request, hands the raw request text to the test, and
/// answers with a fixed status and body.
struct TestEndpoint {
    port: u16,
    received: std::sync::mpsc::Receiver<String>,
}

impl TestEndpoint {
    fn start(status: &str, body: &str) -> Self {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, received) = std::sync::mpsc::channel();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

            let mut raw = Vec::new();
            let mut chunk = [0_u8; 1024];
            // Reading stops on the read timeout once the body is in, so a
            // request without a Content-Length still gets collected whole.
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                let Some(head) = find_header_end(&raw) else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&raw[..head]).to_string();
                if raw.len() >= head + 4 + content_length(&headers) {
                    break;
                }
            }

            let _ = tx.send(String::from_utf8_lossy(&raw).to_string());
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });

        Self { port, received }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The request the hook sent, waiting for it to arrive.
    fn request(&self) -> String {
        self.received
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the hook should have sent a request")
    }
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

fn http_config(event: &str, url: &str) -> HookConfig {
    http_config_with(event, url, HashMap::new(), Vec::new())
}

fn http_config_with(
    event: &str,
    url: &str,
    headers: HashMap<String, String>,
    allowed_env_vars: Vec<String>,
) -> HookConfig {
    HookConfig {
        watch: None,
        event: event.to_string(),
        kind: HookType::Http,
        command: String::new(),
        http: Some(coderex_hooks::HttpHook {
            url: url.to_string(),
            headers,
            allowed_env_vars,
            block_private_hosts: false,
        }),
        prompt: None,
        model: None,
        mcp: None,
        env: HashMap::new(),
        cwd: None,
        internal: false,
        timeout_ms: 5000,
        priority: 0,
        dry_run: false,
        stream: false,
        max_cpu_seconds: None,
        max_memory_mb: None,
        max_file_size_mb: None,
        sandbox: None,
        tier: HookTier::Core,
    }
}

#[tokio::test]
async fn an_http_hook_posts_its_input_and_continues() {
    let endpoint = TestEndpoint::start("200 OK", r#"{"action":"continue"}"#);
    let mut registry = HookRegistry::new();
    registry.register(http_config("PreToolUse", &endpoint.url("/hook")));

    let result = registry
        .fire_hook("PreToolUse", "Bash", &HashMap::new())
        .await;

    assert!(!result.stop, "a continue answer must not stop the tool");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let request = endpoint.request();
    assert!(request.starts_with("POST /hook"), "{request}");
    assert!(
        request.contains("PreToolUse"),
        "the hook input was not posted: {request}"
    );
}

#[tokio::test]
async fn an_http_hook_that_answers_stop_stops_a_gating_event() {
    let endpoint = TestEndpoint::start("200 OK", r#"{"action":"stop"}"#);
    let mut registry = HookRegistry::new();
    registry.register(http_config("PreToolUse", &endpoint.url("/hook")));

    let result = registry
        .fire_hook("PreToolUse", "Bash", &HashMap::new())
        .await;

    assert!(result.stop, "an endpoint that said stop was ignored");
    assert!(
        !endpoint.request().is_empty(),
        "the hook never reached the endpoint, so this test proves nothing about the answer"
    );
}

/// A status that is not a success is a hook that did not answer, and a gating
/// event denies on that.
#[tokio::test]
async fn an_http_hook_endpoint_that_fails_denies() {
    let endpoint = TestEndpoint::start("500 Internal Server Error", "boom");
    let mut registry = HookRegistry::new();
    registry.register(http_config("PreToolUse", &endpoint.url("/hook")));

    let result = registry
        .fire_hook("PreToolUse", "Bash", &HashMap::new())
        .await;

    assert!(result.stop, "a 500 was read as an allow");
    assert!(!result.errors.is_empty());
    assert!(
        !endpoint.request().is_empty(),
        "the hook never reached the endpoint, so this test proves nothing about the answer"
    );
}

#[tokio::test]
async fn an_http_hook_that_says_nothing_denies() {
    let endpoint = TestEndpoint::start("200 OK", "");
    let mut registry = HookRegistry::new();
    registry.register(http_config("PreToolUse", &endpoint.url("/hook")));

    let result = registry
        .fire_hook("PreToolUse", "Bash", &HashMap::new())
        .await;

    assert!(result.stop, "an empty body was read as an allow");
    assert!(
        !endpoint.request().is_empty(),
        "the hook never reached the endpoint, so this test proves nothing about the answer"
    );
}

#[tokio::test]
async fn an_http_hook_that_answers_nonsense_denies() {
    let endpoint = TestEndpoint::start("200 OK", "not json at all");
    let mut registry = HookRegistry::new();
    registry.register(http_config("PreToolUse", &endpoint.url("/hook")));

    let result = registry
        .fire_hook("PreToolUse", "Bash", &HashMap::new())
        .await;

    assert!(result.stop, "unparseable body was read as an allow");
    assert!(
        !endpoint.request().is_empty(),
        "the hook never reached the endpoint, so this test proves nothing about the answer"
    );
}

/// A redirect is a second destination the configuration did not name, and the
/// body is the conversation — so it is a failure, not something to follow.
#[tokio::test]
async fn an_http_hook_redirect_is_a_failure() {
    let endpoint = TestEndpoint::start("302 Found", "");
    let mut registry = HookRegistry::new();
    // The endpoint answers 302; if the client followed redirects it would look
    // for a Location target and hang instead of failing.
    registry.register(http_config("PreToolUse", &endpoint.url("/hook")));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        registry.fire_hook("PreToolUse", "Bash", &HashMap::new()),
    )
    .await
    .expect("the hook should fail rather than follow the redirect");

    assert!(result.stop);
    let _ = endpoint.request();
}

#[tokio::test]
async fn an_http_hook_sends_a_header_it_was_granted() {
    let endpoint = TestEndpoint::start("200 OK", r#"{"action":"continue"}"#);
    let mut registry = HookRegistry::new();
    let mut headers = HashMap::new();
    headers.insert("authorization".to_string(), "Bearer ${PATH}".to_string());
    registry.register(http_config_with(
        "PreToolUse",
        &endpoint.url("/hook"),
        headers,
        vec!["PATH".to_string()],
    ));

    let result = registry
        .fire_hook("PreToolUse", "Bash", &HashMap::new())
        .await;
    assert!(!result.stop, "{:?}", result.errors);

    let request = endpoint.request();
    let path = std::env::var("PATH").unwrap_or_default();
    assert!(
        request.contains(&format!("authorization: Bearer {path}")),
        "the granted variable was not interpolated: {request}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Action kinds — `prompt`, `agent`, `mcp_tool`
//
//  These need a model client or the MCP registry, which the hooks crate does
//  not own: the session injects a `HookActionRunner`. The runner below is
//  implemented here as an *external* crate would implement it, which is half of
//  what these tests pin — the other half is that the three kinds answer through
//  the same parser, and that a kind with no runner denies on a gate.
// ═══════════════════════════════════════════════════════════════════════════

/// A runner that answers from a script and records what it was asked.
struct RecordingRunner {
    answer: String,
    prompts: std::sync::Mutex<Vec<coderex_hooks::ActionPrompt>>,
    mcp_calls: std::sync::Mutex<Vec<(String, String, serde_json::Value)>>,
}

impl RecordingRunner {
    fn answering(answer: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            answer: answer.to_string(),
            prompts: std::sync::Mutex::new(Vec::new()),
            mcp_calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> Vec<coderex_hooks::ActionPrompt> {
        self.prompts.lock().unwrap().clone()
    }

    fn mcp_calls(&self) -> Vec<(String, String, serde_json::Value)> {
        self.mcp_calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl coderex_hooks::HookActionRunner for RecordingRunner {
    async fn answer(
        &self,
        request: &coderex_hooks::ActionPrompt,
    ) -> Result<String, coderex_hooks::HookError> {
        self.prompts.lock().unwrap().push(request.clone());
        Ok(self.answer.clone())
    }

    async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<String, coderex_hooks::HookError> {
        self.mcp_calls
            .lock()
            .unwrap()
            .push((server.to_string(), tool.to_string(), input.clone()));
        Ok(self.answer.clone())
    }
}

/// A `prompt` hook, an `agent` hook, and an `mcp_tool` hook all answer through
/// the installed runner, and each one's answer is parsed the same way.
#[tokio::test]
async fn the_action_hook_kinds_dispatch_to_the_installed_runner() {
    let hooks = serde_json::json!([
        { "event": "PreToolUse", "type": "prompt", "prompt": "gate: $ARGUMENTS" },
        { "event": "PreToolUse", "type": "agent", "prompt": "verify: $ARGUMENTS" },
        {
            "event": "PreToolUse", "type": "mcp_tool",
            "server": "policy", "tool": "check",
            "input": { "path": "${tool_input.file_path}" }
        }
    ]);

    let runner =
        RecordingRunner::answering(r#"{"action":"continue","additional_context":"answered"}"#);
    let mut registry = HookRegistry::from_settings_json(&hooks).unwrap();
    registry.set_action_runner(runner.clone());
    let executor = registry.into_executor();

    let result = executor
        .fire_hook(
            "PreToolUse",
            r#"{"file_path":"/etc/hosts"}"#,
            &HashMap::new(),
        )
        .await;

    assert!(
        !result.failed,
        "the runner answered for every kind: {:?}",
        result.errors
    );
    assert!(!result.stop);

    let prompts = runner.prompts();
    assert_eq!(prompts.len(), 2, "one query per model-backed hook");
    assert!(
        prompts.iter().any(|p| !p.agent) && prompts.iter().any(|p| p.agent),
        "the `agent` kind must be distinguishable from the `prompt` kind"
    );
    assert!(prompts.iter().all(|p| p.prompt.contains("PreToolUse")));

    let calls = runner.mcp_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "policy");
    assert_eq!(calls[0].1, "check");
    assert_eq!(calls[0].2, serde_json::json!({ "path": "/etc/hosts" }));
}

/// A hook of a kind this session cannot run is a failure, and on a gating event
/// a failure denies. The check is that the tool would have been *stopped*, not
/// that no error was logged.
#[tokio::test]
async fn an_action_hook_with_no_runner_denies_on_a_gate() {
    let hooks = serde_json::json!([
        { "event": "PreToolUse", "type": "prompt", "prompt": "is this ok?" }
    ]);
    let registry = HookRegistry::from_settings_json(&hooks).unwrap();

    let result = registry
        .fire_hook("PreToolUse", "Bash", &HashMap::new())
        .await;

    assert!(result.failed, "an unrunnable gate is a failure");
    assert!(result.stop, "an unrunnable gate must deny");
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("this session cannot run prompt hooks")),
        "the reason must name the kind and the fix: {:?}",
        result.errors
    );
}

/// A `${tool_input.<path>}` reference that does not resolve is a failure — and
/// on a gate, a denial, not a call to the tool with an empty argument.
#[tokio::test]
async fn an_unresolvable_mcp_tool_input_denies_on_a_gate() {
    let hooks = serde_json::json!([
        {
            "event": "PreToolUse", "type": "mcp_tool",
            "server": "policy", "tool": "check",
            "input": { "path": "${tool_input.no_such_field}" }
        }
    ]);
    let runner = RecordingRunner::answering(r#"{"action":"continue"}"#);
    let mut registry = HookRegistry::from_settings_json(&hooks).unwrap();
    registry.set_action_runner(runner.clone());

    let result = registry
        .fire_hook(
            "PreToolUse",
            r#"{"file_path":"/etc/hosts"}"#,
            &HashMap::new(),
        )
        .await;

    assert!(result.stop, "an unresolvable argument must deny the tool");
    assert!(runner.mcp_calls().is_empty(), "the tool must not be called");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Streaming protocol — events, logs, and host-answered requests
// ═══════════════════════════════════════════════════════════════════════════

/// A registry with one `stream: true` command hook.
fn streaming_registry(command: &str) -> HookRegistry {
    let hooks = serde_json::json!([{
        "event": "PreToolUse",
        "command": command,
        "stream": true,
        "timeout_ms": 5000,
    }]);
    HookRegistry::from_settings_json(&hooks).unwrap()
}

/// A path unique to this test process.
fn scratch_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("coderex-{name}-{}.tmp", std::process::id()))
}

#[tokio::test]
async fn a_streaming_hook_reports_events_before_its_answer() {
    let registry = streaming_registry(
        r#"echo '{"type":"event","event":"step","data":{"n":1}}'
           echo '{"type":"log","message":"working"}'
           echo '{"action":"continue"}'"#,
    );

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();

    assert_eq!(
        result.events,
        vec![HookStreamEvent {
            event: "step".into(),
            data: Some(serde_json::json!({"n": 1})),
        }]
    );
    assert!(matches!(result.hook_result.action, HookAction::Continue));
}

#[derive(Debug)]
struct AnsweringHandler;

impl HookRequestHandler for AnsweringHandler {
    fn handle(
        &self,
        method: &str,
        _params: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, HookRequestError> {
        assert_eq!(method, "context");
        Ok(serde_json::json!({"answer": 42}))
    }
}

/// The hook asks a question, waits on stdin, and records the answer it got.
///
/// The first line of stdin is the `HookInput`, exactly as in the one-way
/// contract; host messages (here the response) follow it.
fn asking_command(path: &std::path::Path) -> String {
    r#"IFS= read -r initial_input
       printf '%s\n' '{"type":"request","id":7,"method":"context","params":{}}'
       IFS= read -r answer
       printf '%s' "$answer" > __ANSWER_PATH__
       printf '%s\n' '{"action":"continue"}'"#
        .replace("__ANSWER_PATH__", &path.display().to_string())
}

#[tokio::test]
async fn a_streaming_hook_gets_an_answer_to_its_request() {
    let answer_path = scratch_path("hook-answer");
    let _ = std::fs::remove_file(&answer_path);
    let mut registry = streaming_registry(&asking_command(&answer_path));
    registry.set_request_handler(std::sync::Arc::new(AnsweringHandler));

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));

    let answer = std::fs::read_to_string(&answer_path).unwrap();
    let _ = std::fs::remove_file(&answer_path);
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();

    assert_eq!(answer["type"], "response");
    assert_eq!(answer["id"], 7);
    assert_eq!(answer["result"]["answer"], 42);
}

#[tokio::test]
async fn a_streaming_hook_with_no_handler_is_refused_not_left_waiting() {
    let answer_path = scratch_path("hook-refusal");
    let _ = std::fs::remove_file(&answer_path);
    let registry = streaming_registry(&asking_command(&answer_path));

    let results = registry
        .execute_sync(&HookEvent::PreToolUse, &input("PreToolUse"))
        .await;
    let result = results[0].as_ref().unwrap();
    assert!(matches!(result.hook_result.action, HookAction::Continue));

    let answer = std::fs::read_to_string(&answer_path).unwrap();
    let _ = std::fs::remove_file(&answer_path);
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();

    assert_eq!(answer["type"], "response");
    assert_eq!(answer["id"], 7);
    assert_eq!(answer["error"]["code"], -32601);
}
