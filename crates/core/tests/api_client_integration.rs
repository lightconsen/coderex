//! API Client integration tests — covering TokenBudget, CostTracker, and ModelRouter.
//!
//! These tests correspond to the equivalence test plan Module 10:
//! 10.1 streaming, 10.2 retry/fallback, 10.3 TokenBudget, 10.4 CostTracker, 10.5 model routing

use std::pin::Pin;

use coderex_core::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use coderex_core::budget::{BudgetThresholds, TokenBudget};
use coderex_core::cost::{CostTracker, ModelPricing, pricing_for_model};
use coderex_core::model_router::ModelRouter;
use coderex_core::provider::{Provider, ProviderCapabilities, ProviderError, ProviderErrorKind};
use coderex_core::types::{ModelId, UsageSummary};
use async_trait::async_trait;
use futures::Stream;
use coderex_core::tool_executor::ToolInfo;

// ═══════════════════════════════════════════════════════════════════════════
//  Mock Providers
// ═══════════════════════════════════════════════════════════════════════════

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
    router.add_provider(Box::new(MockProvider::failing("fail1", ProviderErrorKind::Unavailable)));
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
    router.add_provider(Box::new(MockProvider::failing("fail1", ProviderErrorKind::Auth)));
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
    router.add_provider(Box::new(MockProvider::new("fallback").with_response("from-fallback")));

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
    router.add_provider(Box::new(MockProvider::failing("fail1", ProviderErrorKind::Unavailable)));
    router.add_provider(Box::new(MockProvider::failing("fail2", ProviderErrorKind::Unavailable)));
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
    router.add_provider(Box::new(MockProvider::failing("fail1", ProviderErrorKind::Unavailable)));
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
