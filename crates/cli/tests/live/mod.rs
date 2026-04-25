//! Live LLM E2E test harness — runs against a real API.
//!
//! Set environment variables before running:
//!   source start.kimi.sh
//!   cargo test -p coderex-cli -- --ignored

use coderex_core::api_client::{AnthropicApiClient, ApiConfig};
use coderex_core::permissions::PermissionMode;
use coderex_core::types::ModelId;
use coderex_test_utils::{TestRig, TestRigBuilder};
use coderex_tools::{
    ExecutionConfig, ExecutionContext, RegistryToolDispatcher, ToolExecutor, ToolRegistry,
    register_builtin_tools,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Check if live API credentials are available.
#[must_use]
pub fn have_live_env() -> bool {
    (std::env::var("ANTHROPIC_API_KEY").is_ok() && std::env::var("ANTHROPIC_BASE_URL").is_ok())
        || (std::env::var("OPENAI_API_KEY").is_ok() && std::env::var("OPENAI_BASE_URL").is_ok())
}

/// Skip message when env is missing.
#[must_use]
pub fn skip_msg() -> &'static str {
    "ANTHROPIC_API_KEY+ANTHROPIC_BASE_URL or OPENAI_API_KEY+OPENAI_BASE_URL not set"
}

/// Build a real API client from environment.
#[must_use]
pub fn live_api_client() -> AnthropicApiClient {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .expect("ANTHROPIC_API_KEY or OPENAI_API_KEY not set");
    let base = std::env::var("ANTHROPIC_BASE_URL")
        .or_else(|_| std::env::var("OPENAI_BASE_URL"))
        .expect("ANTHROPIC_BASE_URL or OPENAI_BASE_URL not set");
    let mut config = ApiConfig::new(key);
    config.base_url = base;
    config.request_timeout_secs = 180; // longer timeout for live tests
    AnthropicApiClient::new(config)
}

/// Build a model ID from environment or default.
#[must_use]
pub fn live_model() -> ModelId {
    let model = std::env::var("ANTHROPIC_MODEL")
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "claude-sonnet-4-6".into());
    ModelId(model)
}

/// Build a TestRig with real API client and real builtin tools.
pub async fn live_rig() -> TestRig {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);
    let registry = Arc::new(registry);
    let executor = Arc::new(ToolExecutor::new(ExecutionConfig::default()));
    let context = ExecutionContext::Normal;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dispatcher = RegistryToolDispatcher::new(registry, executor, context, cwd);

    TestRigBuilder::new()
        .with_api_client(Box::new(live_api_client()))
        .with_tool_executor(Box::new(dispatcher))
        .with_permission_mode(PermissionMode::Auto)
        .with_model(live_model())
        .build()
        .await
}

/// Collect all text from TextDelta events.
#[must_use]
pub fn collect_text(events: &[coderex_core::types::EngineEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            coderex_core::types::EngineEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// Check if any event matches a tool call by name.
#[must_use]
#[allow(dead_code)]
pub fn has_tool_call(events: &[coderex_core::types::EngineEvent], name: &str) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            coderex_core::types::EngineEvent::ToolCallStart { name: n, .. } if n == name
        )
    })
}

/// Check if a tool call completed successfully.
#[must_use]
#[allow(dead_code)]
pub fn has_tool_call_success(events: &[coderex_core::types::EngineEvent], name: &str) -> bool {
    events.iter().any(|e| match e {
        coderex_core::types::EngineEvent::ToolCallEnd { result, .. } => {
            result.tool_call_id.contains(name) || !result.is_error
        }
        _ => false,
    })
}
