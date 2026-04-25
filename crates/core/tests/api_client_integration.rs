//! API Client integration tests — covering TokenBudget, CostTracker, and ModelRouter.
//!
//! These tests correspond to the equivalence test plan Module 10:
//! 10.1 streaming, 10.2 retry/fallback, 10.3 TokenBudget, 10.4 CostTracker, 10.5 model routing

use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use coderex_core::api_client::{
    AnthropicApiClient, ApiClient, ApiConfig, ApiError, ApiResponse, RequestContext, StreamEvent,
};
use coderex_core::budget::{BudgetThresholds, TokenBudget};
use coderex_core::cost::{CostTracker, ModelPricing, pricing_for_model};
use coderex_core::model_router::ModelRouter;
use coderex_core::provider::{Provider, ProviderCapabilities, ProviderError, ProviderErrorKind};
use coderex_core::provider_router::ProviderRouter;
use coderex_core::providers::OpenAiCompatibleProvider;
use coderex_core::tool_executor::ToolInfo;
use coderex_core::types::{ModelId, UsageSummary};
use futures::{Stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ═══════════════════════════════════════════════════════════════════════════
//  Mock Providers
// ═══════════════════════════════════════════════════════════════════════════

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug)]
struct CapturedRequest {
    headers: std::collections::HashMap<String, String>,
    body: serde_json::Value,
}

async fn read_captured_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let n = socket.read(&mut chunk).await.expect("read request");
        assert!(n > 0, "request closed before headers");
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let _request_line = lines.next().unwrap_or_default();
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + content_length {
        let mut chunk = vec![0u8; content_length.max(1)];
        let n = socket.read(&mut chunk).await.expect("read body");
        assert!(n > 0, "request closed before body");
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body_bytes = &buffer[header_end..header_end + content_length];
    let body: serde_json::Value = serde_json::from_slice(body_bytes).expect("json body");

    CapturedRequest { headers, body }
}

async fn capture_single_request(
    response_body: serde_json::Value,
) -> (String, tokio::task::JoinHandle<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let captured = read_captured_request(&mut socket).await;

        let response_text = response_body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_text.len(),
            response_text
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        socket.shutdown().await.expect("shutdown");

        captured
    });
    (format!("http://{}", addr), handle)
}

async fn capture_requests_with_raw_responses(
    responses: Vec<String>,
) -> (String, tokio::task::JoinHandle<Vec<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let mut captured = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            captured.push(read_captured_request(&mut socket).await);
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            socket.shutdown().await.expect("shutdown");
        }
        captured
    });
    (format!("http://{}", addr), handle)
}

struct MockProvider {
    name: String,
    supported_models: Vec<String>,
    response: Option<String>,
    error_kind: Option<ProviderErrorKind>,
}

impl MockProvider {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            supported_models: vec![],
            response: Some("success".into()),
            error_kind: None,
        }
    }

    fn failing(name: &str, kind: ProviderErrorKind) -> Self {
        Self {
            name: name.into(),
            supported_models: vec![],
            response: None,
            error_kind: Some(kind),
        }
    }

    fn with_models(mut self, models: Vec<&str>) -> Self {
        self.supported_models = models.into_iter().map(|s| s.into()).collect();
        self
    }

    fn with_response(mut self, response: &str) -> Self {
        self.response = Some(response.into());
        self
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            structured_tool_streaming: true,
            parallel_tool_calls: true,
            text_tool_fallback: true,
            prompt_caching: false,
            vision: false,
        }
    }

    async fn create_message(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Result<ApiResponse, ProviderError> {
        if let Some(ref kind) = self.error_kind {
            return Err(ProviderError {
                message: format!("{} failed", self.name),
                kind: kind.clone(),
                provider: self.name.clone(),
            });
        }
        Ok(ApiResponse {
            content: self.response.clone().unwrap_or_default(),
            tool_calls: vec![],
            usage: UsageSummary {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            stop_reason: "end_turn".into(),
        })
    }

    async fn create_message_streaming(
        &self,
        _system: &str,
        _messages: &[serde_json::Value],
        _model: &ModelId,
        _max_tokens: u32,
        _tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        if self.error_kind.is_some() {
            return Box::pin(futures::stream::once(async {
                StreamEvent::Error(ApiError::Http("stream failed".into()))
            }));
        }
        let content = self.response.clone().unwrap_or_default();
        Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta(content.clone()),
            StreamEvent::StreamEnd,
        ]))
    }

    fn resolve_model(&self, model: &ModelId) -> String {
        model.0.clone()
    }

    fn supports_model(&self, model: &ModelId) -> bool {
        if self.supported_models.is_empty() {
            true
        } else {
            self.supported_models.contains(&model.0)
        }
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(Self {
            name: self.name.clone(),
            supported_models: self.supported_models.clone(),
            response: self.response.clone(),
            error_kind: self.error_kind.clone(),
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  10.3 TokenBudget — threshold detection and stopping
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn token_budget_records_tokens_across_turns() {
    let mut budget = TokenBudget::with_defaults();

    let usage = UsageSummary {
        input_tokens: 0,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    budget.record_tokens(&usage);
    assert_eq!(budget.turn_tokens, 100);
    assert_eq!(budget.session_tokens, 100);

    budget.record_tokens(&usage);
    assert_eq!(budget.turn_tokens, 200);
    assert_eq!(budget.session_tokens, 200);
}

#[test]
fn token_budget_should_continue_respects_turn_limit() {
    let thresholds = BudgetThresholds {
        max_tokens_per_turn: 1000,
        max_tokens_per_session: 1_000_000,
        max_continuations: 5,
        diminishing_returns_ratio: 0.1,
    };
    let mut budget = TokenBudget::new(thresholds);

    let usage = UsageSummary {
        input_tokens: 0,
        output_tokens: 500,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };
    budget.record_tokens(&usage);
    assert!(budget.should_continue());

    budget.record_tokens(&usage);
    assert!(!budget.should_continue());
}

#[test]
fn token_budget_should_continue_respects_session_limit() {
    let thresholds = BudgetThresholds {
        max_tokens_per_turn: 1_000_000,
        max_tokens_per_session: 500,
        max_continuations: 5,
        diminishing_returns_ratio: 0.1,
    };
    let mut budget = TokenBudget::new(thresholds);

    let usage = UsageSummary {
        input_tokens: 0,
        output_tokens: 500,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };
    budget.record_tokens(&usage);
    assert!(!budget.should_continue());
}

#[test]
fn token_budget_continuation_limit() {
    let thresholds = BudgetThresholds {
        max_tokens_per_turn: 1_000_000,
        max_tokens_per_session: 1_000_000,
        max_continuations: 2,
        diminishing_returns_ratio: 0.1,
    };
    let mut budget = TokenBudget::new(thresholds);

    let usage = UsageSummary {
        input_tokens: 0,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    budget.record_tokens(&usage);
    budget.record_continuation();
    assert!(budget.should_continue());

    budget.record_tokens(&usage);
    budget.record_continuation();
    assert!(!budget.should_continue());
}

#[test]
fn token_budget_diminishing_returns() {
    let thresholds = BudgetThresholds {
        max_tokens_per_turn: 1_000_000,
        max_tokens_per_session: 1_000_000,
        max_continuations: 5,
        diminishing_returns_ratio: 0.1,
    };
    let mut budget = TokenBudget::new(thresholds);

    let large = UsageSummary {
        input_tokens: 0,
        output_tokens: 1000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };
    let tiny = UsageSummary {
        input_tokens: 0,
        output_tokens: 10,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    budget.record_tokens(&large);
    budget.record_continuation();
    assert!(!budget.is_diminishing_returns());

    budget.record_tokens(&tiny);
    assert!(budget.is_diminishing_returns());
}

#[test]
fn token_budget_reset_turn_clears() {
    let mut budget = TokenBudget::with_defaults();
    let usage = UsageSummary {
        input_tokens: 0,
        output_tokens: 500,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };
    budget.record_tokens(&usage);
    budget.record_continuation();

    budget.reset_turn();

    assert_eq!(budget.turn_tokens, 0);
    assert_eq!(budget.continuation_count, 0);
    assert_eq!(budget.last_delta_tokens, 0);
    // Session tokens should persist
    assert_eq!(budget.session_tokens, 500);
}

// ═══════════════════════════════════════════════════════════════════════════
//  10.4 CostTracker — accumulation, agent/task costs, turn history
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn cost_tracker_accumulates_per_turn() {
    let mut tracker = CostTracker::new();
    let usage = UsageSummary {
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    tracker.start_turn();
    tracker.record_usage(&usage, &ModelId::sonnet());
    let turn1_cost = tracker.get_turn_cost();
    tracker.end_turn(1, &usage, &ModelId::sonnet());

    tracker.start_turn();
    tracker.record_usage(&usage, &ModelId::sonnet());
    let turn2_cost = tracker.get_turn_cost();

    assert!((turn1_cost - turn2_cost).abs() < 1e-9);
    assert!((tracker.get_total_cost() - 2.0 * turn1_cost).abs() < 1e-9);
}

#[test]
fn cost_tracker_pricing_differs_by_model() {
    let mut tracker = CostTracker::new();
    let usage = UsageSummary {
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    tracker.start_turn();
    tracker.record_usage(&usage, &ModelId::sonnet());
    let sonnet_cost = tracker.get_turn_cost();
    tracker.end_turn(1, &usage, &ModelId::sonnet());

    tracker.start_turn();
    tracker.record_usage(&usage, &ModelId::opus());
    let opus_cost = tracker.get_turn_cost();

    // Opus is more expensive than Sonnet
    assert!(opus_cost > sonnet_cost);
}

#[test]
fn cost_tracker_agent_costs_separate() {
    let mut tracker = CostTracker::new();
    tracker.record_agent_cost("agent_1", 0.50);
    tracker.record_agent_cost("agent_1", 0.25);
    tracker.record_agent_cost("agent_2", 1.00);

    assert!((tracker.get_agent_cost("agent_1") - 0.75).abs() < 1e-9);
    assert!((tracker.get_agent_cost("agent_2") - 1.00).abs() < 1e-9);
    assert!(tracker.get_agent_cost("agent_3") < 1e-9);
}

#[test]
fn cost_tracker_task_costs() {
    let mut tracker = CostTracker::new();
    tracker.record_task_cost("task_a", 0.10);
    tracker.record_task_cost("task_a", 0.20);

    assert!((tracker.get_task_cost("task_a") - 0.30).abs() < 1e-9);
}

#[test]
fn cost_tracker_turn_history() {
    let mut tracker = CostTracker::new();
    let usage = UsageSummary {
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    for i in 1..=3 {
        tracker.start_turn();
        tracker.record_usage(&usage, &ModelId::sonnet());
        tracker.end_turn(i, &usage, &ModelId::sonnet());
    }

    let history = tracker.get_turn_history();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].turn_number, 1);
    assert_eq!(history[2].turn_number, 3);
}

#[test]
fn cost_tracker_format_summary() {
    let mut tracker = CostTracker::new();
    let usage = UsageSummary {
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    tracker.start_turn();
    tracker.record_usage(&usage, &ModelId::sonnet());
    tracker.end_turn(1, &usage, &ModelId::sonnet());

    let summary = tracker.format_summary();
    assert!(summary.contains("Session cost: $"));
    assert!(summary.contains("1 turns"));
}

#[test]
fn cost_tracker_with_cache_tokens() {
    let mut tracker = CostTracker::new();
    let usage = UsageSummary {
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_tokens: 2000,
        cache_creation_tokens: 500,
    };

    tracker.start_turn();
    tracker.record_usage(&usage, &ModelId::sonnet());
    let cost = tracker.get_turn_cost();

    // Cost should include cache tokens
    let pricing = ModelPricing::sonnet();
    let expected = pricing.calculate(&usage);
    assert!((cost - expected).abs() < 1e-9);
}

#[test]
fn pricing_for_model_defaults() {
    let unknown = ModelId("unknown-model".into());
    let p = pricing_for_model(&unknown);
    assert_eq!(p.input_per_million, 3.0); // Sonnet default
}

#[test]
fn pricing_for_opus() {
    let p = pricing_for_model(&ModelId::opus());
    assert_eq!(p.input_per_million, 15.0);
}

#[test]
fn pricing_for_haiku() {
    let p = pricing_for_model(&ModelId::haiku());
    assert_eq!(p.input_per_million, 0.8);
}

// ═══════════════════════════════════════════════════════════════════════════
//  10.5 ModelRouter — provider selection and fallback
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn model_router_falls_back_on_failure() {
    let mut router = ModelRouter::new("test");
    router.add_provider(Box::new(MockProvider::failing(
        "fail1",
        ProviderErrorKind::Unavailable,
    )));
    router.add_provider(Box::new(MockProvider::new("success1")));

    let result = router
        .create_message("", &[], &ModelId::sonnet(), 1024, &[])
        .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap().content, "success");
}

#[tokio::test]
async fn model_router_non_fallback_error_stops() {
    let mut router = ModelRouter::new("test");
    router.add_provider(Box::new(MockProvider::failing(
        "fail1",
        ProviderErrorKind::Auth,
    )));
    router.add_provider(Box::new(MockProvider::new("success1")));

    let result = router
        .create_message("", &[], &ModelId::sonnet(), 1024, &[])
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn model_router_no_providers_fails() {
    let router = ModelRouter::new("empty");

    let result = router
        .create_message("", &[], &ModelId::sonnet(), 1024, &[])
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn model_router_model_filtered_providers() {
    let mut router = ModelRouter::new("test");
    router.add_provider_for_models(
        Box::new(MockProvider::new("sonnet-only")),
        vec!["claude-sonnet".into()],
    );
    router.add_provider(Box::new(
        MockProvider::new("fallback").with_response("from-fallback"),
    ));

    // Model that doesn't match the filter should still hit fallback
    let result = router
        .create_message("", &[], &ModelId("claude-opus-4-6".into()), 1024, &[])
        .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap().content, "from-fallback");
}

#[tokio::test]
async fn model_router_max_fallbacks_respected() {
    let mut router = ModelRouter::new("test").with_max_fallbacks(1);
    router.add_provider(Box::new(MockProvider::failing(
        "fail1",
        ProviderErrorKind::Unavailable,
    )));
    router.add_provider(Box::new(MockProvider::failing(
        "fail2",
        ProviderErrorKind::Unavailable,
    )));
    router.add_provider(Box::new(MockProvider::new("success1")));

    let result = router
        .create_message("", &[], &ModelId::sonnet(), 1024, &[])
        .await;

    // Only 1 fallback attempt, so we exhaust after fail1 -> fail2
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
//  10.1 Streaming — SSE stream consumption via mock provider
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn streaming_delivers_events() {
    let provider = MockProvider::new("streamer");
    let stream = provider
        .create_message_streaming("", &[], &ModelId::sonnet(), 1024, &[])
        .await;

    let events: Vec<StreamEvent> = futures::StreamExt::collect(stream).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], StreamEvent::TextDelta(ref s) if s == "success"));
    assert!(matches!(events[1], StreamEvent::StreamEnd));
}

#[tokio::test]
async fn streaming_error_event() {
    let provider = MockProvider::failing("fail", ProviderErrorKind::Unavailable);
    let stream = provider
        .create_message_streaming("", &[], &ModelId::sonnet(), 1024, &[])
        .await;

    let events: Vec<StreamEvent> = futures::StreamExt::collect(stream).await;
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], StreamEvent::Error(_)));
}

#[tokio::test]
async fn model_router_streaming_fallback() {
    let mut router = ModelRouter::new("test");
    router.add_provider(Box::new(MockProvider::failing(
        "fail1",
        ProviderErrorKind::Unavailable,
    )));
    router.add_provider(Box::new(MockProvider::new("success1")));

    let stream = router
        .create_message_streaming("", &[], &ModelId::sonnet(), 1024, &[])
        .await;

    let events: Vec<StreamEvent> = futures::StreamExt::collect(stream).await;
    // First provider emits error, second emits text + done
    assert!(!events.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: budget + cost + router integration
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn budget_and_cost_tracker_session_lifecycle() {
    let mut budget = TokenBudget::with_defaults();
    let mut tracker = CostTracker::new();

    // Turn 1
    let usage1 = UsageSummary {
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    tracker.start_turn();
    budget.record_tokens(&usage1);
    tracker.record_usage(&usage1, &ModelId::sonnet());
    tracker.end_turn(1, &usage1, &ModelId::sonnet());

    assert!(budget.should_continue());
    assert!(tracker.get_total_cost() > 0.0);

    // Turn 2
    budget.reset_turn();
    tracker.start_turn();

    let usage2 = UsageSummary {
        input_tokens: 2000,
        output_tokens: 1000,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };
    budget.record_tokens(&usage2);
    tracker.record_usage(&usage2, &ModelId::sonnet());
    tracker.end_turn(2, &usage2, &ModelId::sonnet());

    assert_eq!(tracker.turn_count(), 2);
    assert!(tracker.get_total_cost() > tracker.get_turn_cost());
    assert_eq!(budget.session_tokens, 1500); // 500 + 1000
}

#[test]
fn custom_pricing_override() {
    let mut tracker = CostTracker::new();
    tracker.register_pricing(
        "custom-model".into(),
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_read_per_million: 0.1,
            cache_creation_per_million: 0.5,
        },
    );

    let usage = UsageSummary {
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };

    tracker.start_turn();
    tracker.record_usage(&usage, &ModelId("custom-model".into()));
    let cost = tracker.get_turn_cost();

    assert!((cost - 1.0).abs() < 1e-9);
}

#[tokio::test]
async fn anthropic_client_sends_coderex_identity_headers_and_metadata() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (base_url, handle) = capture_single_request(serde_json::json!({
        "content": [{"type": "text", "text": "ok"}],
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "stop_reason": "end_turn"
    }))
    .await;

    let mut config = ApiConfig::new("test-key".into())
        .with_session_id("sess-test")
        .with_device_id("dev-test");
    config.base_url = base_url;
    let client = AnthropicApiClient::new(config);
    let resp = client
        .create_message("system", &[], &ModelId::sonnet(), 32, &[])
        .await
        .expect("anthropic request should succeed");
    assert_eq!(resp.content, "ok");

    let captured = handle.await.expect("server join");

    assert_eq!(
        captured.headers.get("x-coderex-client").map(String::as_str),
        Some("cli")
    );
    assert_eq!(
        captured
            .headers
            .get("x-coderex-session-id")
            .map(String::as_str),
        Some("sess-test")
    );
    assert!(captured.headers.contains_key("user-agent"));
    let metadata_user_id = captured.body["metadata"]["user_id"]
        .as_str()
        .expect("metadata.user_id string");
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata_user_id).expect("metadata json");
    assert_eq!(metadata_json["session_id"], "sess-test");
    assert_eq!(metadata_json["device_id"], "dev-test");
    assert_eq!(captured.body["messages"], serde_json::json!([]));
}

#[tokio::test]
async fn anthropic_compatible_profile_forwards_structured_tools() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (base_url, handle) = capture_single_request(serde_json::json!({
        "content": [{"type": "text", "text": "ok"}],
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "stop_reason": "end_turn"
    }))
    .await;

    let tool = ToolInfo {
        name: "Read".into(),
        description: "Read a file".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "file_path": { "type": "string" } },
            "required": ["file_path"]
        }),
    };

    let mut config = ApiConfig::new("test-key".into()).with_provider_profile("kimi");
    config.base_url = base_url;
    let client = AnthropicApiClient::new(config);
    let _ = client
        .create_message("system", &[], &ModelId::sonnet(), 32, &[tool])
        .await
        .expect("anthropic-compatible request should succeed");

    let captured = handle.await.expect("server join");
    let tools = captured
        .body
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .expect("anthropic-compatible request should include structured tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "Read");
}

#[tokio::test]
async fn openai_provider_sends_coderex_identity_headers_and_metadata() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (base_url, handle) = capture_single_request(serde_json::json!({
        "choices": [{
            "message": {"content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 2, "completion_tokens": 3}
    }))
    .await;

    let mut headers = std::collections::BTreeMap::new();
    headers.insert("X-Test".to_string(), "1".to_string());
    let provider = OpenAiCompatibleProvider::new("openai", base_url, "test-key")
        .with_provider_profile("dashscope")
        .with_default_headers(headers)
        .with_session_context("sess-openai", "dev-openai")
        .with_request_context(RequestContext {
            session_id: Some("sess-openai".into()),
            device_id: Some("dev-openai".into()),
            turn_id: Some("7".into()),
            agent_id: Some("agent-openai".into()),
            request_id: None,
            provider_hints: Default::default(),
        });
    let resp = provider
        .create_message("system", &[], &ModelId("gpt-4o".into()), 32, &[])
        .await
        .expect("openai request should succeed");
    assert_eq!(resp.content, "ok");

    let captured = handle.await.expect("server join");

    assert_eq!(
        captured.headers.get("x-coderex-client").map(String::as_str),
        Some("cli")
    );
    assert_eq!(
        captured
            .headers
            .get("x-coderex-session-id")
            .map(String::as_str),
        Some("sess-openai")
    );
    assert_eq!(
        captured
            .headers
            .get("x-coderex-agent-id")
            .map(String::as_str),
        Some("agent-openai")
    );
    assert_eq!(
        captured.headers.get("x-test").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        captured
            .headers
            .get("x-dashscope-authtype")
            .map(String::as_str),
        Some("api_key")
    );
    assert!(captured.headers.contains_key("user-agent"));
    let metadata_user_id = captured.body["metadata"]["user_id"]
        .as_str()
        .expect("metadata.user_id string");
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata_user_id).expect("metadata json");
    assert_eq!(metadata_json["session_id"], "sess-openai");
    assert_eq!(metadata_json["device_id"], "dev-openai");
    assert_eq!(metadata_json["turn_id"], "7");
    assert_eq!(
        captured.body["metadata"]["session_context"]["agent_id"],
        "agent-openai"
    );
}

#[tokio::test]
async fn openai_provider_streaming_emits_text_tool_and_usage_events() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sse = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"I will \",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"file\\\":\\\"src/\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"check it\",\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"lib.rs\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n"
    );
    let raw_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
        sse
    );
    let (base_url, handle) = capture_requests_with_raw_responses(vec![raw_response]).await;

    let tool = ToolInfo {
        name: "Read".into(),
        description: "Read a file".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "file": { "type": "string" }
            },
            "required": ["file"]
        }),
    };
    let provider = OpenAiCompatibleProvider::new("openai", base_url, "test-key")
        .with_session_context("sess-stream", "dev-stream");

    let events: Vec<_> = provider
        .create_message_streaming("system", &[], &ModelId("gpt-4o".into()), 32, &[tool])
        .await
        .collect()
        .await;

    assert!(matches!(&events[0], StreamEvent::TextDelta(delta) if delta == "I will "));
    assert!(
        matches!(&events[1], StreamEvent::ToolCallBlockStart { id, name } if id == "call_1" && name == "Read")
    );
    assert!(
        matches!(&events[2], StreamEvent::ToolCallInputDelta { id, delta } if id == "call_1" && delta == "{\"file\":\"src/")
    );
    assert!(matches!(&events[3], StreamEvent::TextDelta(delta) if delta == "check it"));
    assert!(
        matches!(&events[4], StreamEvent::ToolCallInputDelta { id, delta } if id == "call_1" && delta == "lib.rs\"}")
    );
    assert!(
        matches!(&events[5], StreamEvent::ToolCallBlockEnd { id, input } if id == "call_1" && input.is_null())
    );
    assert!(
        matches!(&events[6], StreamEvent::UsageSummary(usage) if usage.input_tokens == 9 && usage.output_tokens == 4)
    );
    assert!(matches!(events.last(), Some(StreamEvent::StreamEnd)));

    let captured = handle.await.expect("server join");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].body["stream"], true);
    assert_eq!(captured[0].body["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn openai_provider_streaming_falls_back_to_non_streaming_request() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let stream_failure = "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops".to_string();
    let json_body = serde_json::json!({
        "choices": [{
            "message": {"content": "fallback ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2}
    });
    let non_stream_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json_body.to_string().len(),
        json_body
    );
    let (base_url, handle) =
        capture_requests_with_raw_responses(vec![stream_failure, non_stream_response]).await;
    let provider = OpenAiCompatibleProvider::new("openai", base_url, "test-key")
        .with_session_context("sess-fallback", "dev-fallback");

    let events: Vec<_> = provider
        .create_message_streaming("system", &[], &ModelId("gpt-4o".into()), 32, &[])
        .await
        .collect()
        .await;

    assert!(matches!(&events[0], StreamEvent::TextDelta(delta) if delta == "fallback ok"));
    assert!(
        matches!(&events[1], StreamEvent::UsageSummary(usage) if usage.input_tokens == 5 && usage.output_tokens == 2)
    );
    assert!(matches!(events.last(), Some(StreamEvent::StreamEnd)));

    let captured = handle.await.expect("server join");
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].body["stream"], true);
    assert!(captured[1].body.get("stream").is_none());
}

#[tokio::test]
async fn openai_responses_provider_uses_responses_payload_shape() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (base_url, handle) = capture_single_request(serde_json::json!({
        "status": "completed",
        "output": [
            {
                "type": "message",
                "content": [{"type": "output_text", "text": "ok"}]
            },
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Read",
                "arguments": "{\"file\":\"src/lib.rs\"}"
            }
        ],
        "usage": {"input_tokens": 2, "output_tokens": 3}
    }))
    .await;

    let tool = ToolInfo {
        name: "Read".into(),
        description: "Read a file".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "file": { "type": "string" } },
            "required": ["file"]
        }),
    };
    let provider = OpenAiCompatibleProvider::new("openai", base_url, "test-key")
        .with_provider_profile("openai-responses")
        .with_request_context(RequestContext {
            session_id: Some("sess-responses".into()),
            device_id: Some("dev-responses".into()),
            turn_id: Some("11".into()),
            agent_id: Some("agent-responses".into()),
            request_id: None,
            provider_hints: Default::default(),
        });

    let resp = provider
        .create_message("system", &[], &ModelId("gpt-4o".into()), 32, &[tool])
        .await
        .expect("responses request should succeed");
    assert_eq!(resp.content, "ok");
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].name, "Read");

    let captured = handle.await.expect("server join");
    assert!(captured.body.get("input").is_some());
    assert!(captured.body.get("messages").is_none());
    assert_eq!(captured.body["tool_choice"], "auto");
    assert_eq!(
        captured.body["metadata"]["session_context"]["turn_id"],
        "11"
    );
}

#[tokio::test]
async fn provider_router_forwards_request_context_to_provider_headers() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (base_url, handle) = capture_single_request(serde_json::json!({
        "choices": [{
            "message": {"content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    }))
    .await;

    let mut model_router = ModelRouter::new("test-router");
    model_router.add_provider(Box::new(OpenAiCompatibleProvider::new(
        "openai", base_url, "test-key",
    )));
    let router = ProviderRouter::from_model_router(model_router);
    let request_client = router.with_request_context(RequestContext {
        session_id: Some("sess-router".into()),
        device_id: Some("dev-router".into()),
        turn_id: Some("3".into()),
        agent_id: Some("agent-router".into()),
        request_id: None,
        provider_hints: Default::default(),
    });

    let resp = request_client
        .create_message("system", &[], &ModelId("gpt-4o".into()), 32, &[])
        .await
        .expect("routed request should succeed");
    assert_eq!(resp.content, "ok");

    let captured = handle.await.expect("server join");
    assert_eq!(
        captured
            .headers
            .get("x-coderex-session-id")
            .map(String::as_str),
        Some("sess-router")
    );
    assert_eq!(
        captured
            .headers
            .get("x-coderex-agent-id")
            .map(String::as_str),
        Some("agent-router")
    );
    assert_eq!(captured.body["metadata"]["session_context"]["turn_id"], "3");
}

#[tokio::test]
async fn provider_errors_surface_request_diagnostics() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let raw_response = "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: 34\r\nx-request-id: srv-123\r\nConnection: close\r\n\r\nonly available for Coding Agents".to_string();
    let (base_url, _handle) = capture_requests_with_raw_responses(vec![raw_response]).await;

    let provider = OpenAiCompatibleProvider::new("openai", base_url, "test-key");
    let err = provider
        .create_message("system", &[], &ModelId("gpt-4o".into()), 32, &[])
        .await
        .expect_err("request should fail");

    assert!(err.message.contains("request_id="));
    assert!(err.message.contains("server_request_id=srv-123"));
}
