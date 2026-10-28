//! Cost tracking — model pricing, per-turn/per-session cost accumulation.

use crate::types::{ModelId, UsageSummary};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Per-model pricing configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Price per 1M input tokens (USD).
    pub input_per_million: f64,
    /// Price per 1M output tokens (USD).
    pub output_per_million: f64,
    /// Price per 1M cache read tokens (USD).
    pub cache_read_per_million: f64,
    /// Price per 1M cache creation tokens (USD).
    pub cache_creation_per_million: f64,
}

impl ModelPricing {
    /// Calculate cost for a usage summary.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn calculate(&self, usage: &UsageSummary) -> f64 {
        let input_cost = usage.input_tokens as f64 / 1_000_000.0 * self.input_per_million;
        let output_cost = usage.output_tokens as f64 / 1_000_000.0 * self.output_per_million;
        let cache_read_cost =
            usage.cache_read_tokens as f64 / 1_000_000.0 * self.cache_read_per_million;
        let cache_creation_cost =
            usage.cache_creation_tokens as f64 / 1_000_000.0 * self.cache_creation_per_million;
        input_cost + output_cost + cache_read_cost + cache_creation_cost
    }
}

/// Known model pricing.
impl ModelPricing {
    /// Claude Sonnet 4.6 pricing (approximate).
    #[must_use]
    pub const fn sonnet() -> Self {
        Self {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: 0.3,
            cache_creation_per_million: 3.75,
        }
    }

    /// Claude Opus 4.6 pricing (approximate).
    #[must_use]
    pub const fn opus() -> Self {
        Self {
            input_per_million: 15.0,
            output_per_million: 75.0,
            cache_read_per_million: 1.5,
            cache_creation_per_million: 18.75,
        }
    }

    /// Claude Haiku 4.5 pricing (approximate).
    #[must_use]
    pub const fn haiku() -> Self {
        Self {
            input_per_million: 0.8,
            output_per_million: 4.0,
            cache_read_per_million: 0.08,
            cache_creation_per_million: 1.0,
        }
    }
}

/// Resolve pricing for a given model.
#[must_use]
pub fn pricing_for_model(model: &ModelId) -> ModelPricing {
    match model.0.as_str() {
        "claude-opus-4-6" => ModelPricing::opus(),
        "claude-haiku-4-5-20251001" => ModelPricing::haiku(),
        _ => ModelPricing::sonnet(),
    }
}

/// Load custom pricing from a JSON file.
///
/// The JSON format is a map of model name to [`ModelPricing`]:
/// ```json
/// {
///   "claude-sonnet-4-6": {
///     "input_per_million": 3.0,
///     "output_per_million": 15.0,
///     "cache_read_per_million": 0.3,
///     "cache_creation_per_million": 3.75
///   }
/// }
/// ```
///
/// Returns an empty map if the file does not exist or cannot be parsed.
#[must_use]
pub fn load_pricing_from_path(path: &Path) -> HashMap<String, ModelPricing> {
    if !path.exists() {
        return HashMap::new();
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to read pricing file {}: {}", path.display(), e);
            return HashMap::new();
        }
    };
    match serde_json::from_str(&content) {
        Ok(pricing) => pricing,
        Err(e) => {
            tracing::warn!("failed to parse pricing file {}: {}", path.display(), e);
            HashMap::new()
        }
    }
}

/// Load pricing from the default user config path (`~/.coderex/pricing.json`).
#[must_use]
pub fn load_user_pricing() -> HashMap<String, ModelPricing> {
    if let Ok(home) = std::env::var("HOME") {
        let path = std::path::PathBuf::from(home)
            .join(".coderex")
            .join("pricing.json");
        return load_pricing_from_path(&path);
    }
    HashMap::new()
}

/// Tracks costs per turn, session, and agent.
#[derive(Debug, Clone)]
pub struct CostTracker {
    /// Pricing overrides for specific models.
    pricing: HashMap<String, ModelPricing>,
    /// Cost accumulated in the current turn.
    turn_cost: f64,
    /// Total cost for the entire session.
    session_cost: f64,
    /// Costs by agent/agent_id.
    agent_costs: HashMap<String, f64>,
    /// Costs by task/task_id.
    task_costs: HashMap<String, f64>,
    /// Cost history per turn (for reporting).
    turn_history: Vec<TurnCost>,
    /// Optional team context to broadcast costs to.
    team_context: Option<Arc<std::sync::Mutex<crate::team_context::TeamContext>>>,
}

/// Cost information for a single turn.
#[derive(Debug, Clone)]
pub struct TurnCost {
    /// Turn number (1-indexed).
    pub turn_number: u32,
    /// Cost in USD for this turn.
    pub cost_usd: f64,
    /// Token usage for this turn.
    pub usage: UsageSummary,
    /// Model used for this turn.
    pub model: String,
}

impl CostTracker {
    /// Create a new cost tracker.
    ///
    /// Attempts to load custom pricing from `~/.coderex/pricing.json`.
    /// Falls back to built-in defaults if the file is missing or invalid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pricing: load_user_pricing(),
            turn_cost: 0.0,
            session_cost: 0.0,
            agent_costs: HashMap::new(),
            task_costs: HashMap::new(),
            turn_history: Vec::new(),
            team_context: None,
        }
    }

    /// Reload pricing from `~/.coderex/pricing.json` at runtime.
    pub fn reload_pricing(&mut self) {
        self.pricing = load_user_pricing();
    }

    /// Set the team context for broadcasting turn costs.
    pub fn set_team_context(
        &mut self,
        context: Arc<std::sync::Mutex<crate::team_context::TeamContext>>,
    ) {
        self.team_context = Some(context);
    }

    /// Register a custom pricing for a model name.
    pub fn register_pricing(&mut self, model_name: String, pricing: ModelPricing) {
        self.pricing.insert(model_name, pricing);
    }

    /// Record usage and calculate cost for the current turn.
    pub fn record_usage(&mut self, usage: &UsageSummary, model: &ModelId) -> f64 {
        let pricing = self
            .pricing
            .get(&model.0)
            .copied()
            .unwrap_or_else(|| pricing_for_model(model));

        let cost = pricing.calculate(usage);
        self.turn_cost += cost;
        self.session_cost += cost;
        cost
    }

    /// End the current turn and record it in history.
    pub fn end_turn(&mut self, turn_number: u32, usage: &UsageSummary, model: &ModelId) {
        let cost = if self.turn_cost > 0.0 {
            self.turn_cost
        } else {
            pricing_for_model(model).calculate(usage)
        };

        self.turn_history.push(TurnCost {
            turn_number,
            cost_usd: cost,
            usage: *usage,
            model: model.0.clone(),
        });

        // Broadcast to team context if present
        if let Some(ref team_ctx) = self.team_context
            && let Ok(mut ctx) = team_ctx.lock()
        {
            ctx.record_activity(
                "cost_tracker".into(),
                format!("Turn {turn_number} cost: ${cost:.4} ({})", model.0),
            );
        }
    }

    /// Start a new turn (reset turn-level cost).
    pub const fn start_turn(&mut self) {
        self.turn_cost = 0.0;
    }

    /// Add cost to an agent.
    pub fn record_agent_cost(&mut self, agent_id: &str, cost: f64) {
        *self.agent_costs.entry(agent_id.into()).or_default() += cost;
    }

    /// Add cost to a task.
    pub fn record_task_cost(&mut self, task_id: &str, cost: f64) {
        *self.task_costs.entry(task_id.into()).or_default() += cost;
    }

    /// Get the total session cost.
    #[must_use]
    pub const fn get_total_cost(&self) -> f64 {
        self.session_cost
    }

    /// Get the current turn cost.
    #[must_use]
    pub const fn get_turn_cost(&self) -> f64 {
        self.turn_cost
    }

    /// Get the cost for a specific agent.
    #[must_use]
    pub fn get_agent_cost(&self, agent_id: &str) -> f64 {
        self.agent_costs.get(agent_id).copied().unwrap_or(0.0)
    }

    /// Get the cost for a specific task.
    #[must_use]
    pub fn get_task_cost(&self, task_id: &str) -> f64 {
        self.task_costs.get(task_id).copied().unwrap_or(0.0)
    }

    /// Get all turn costs.
    #[must_use]
    pub fn get_turn_history(&self) -> &[TurnCost] {
        &self.turn_history
    }

    /// Get the number of turns so far.
    #[must_use]
    pub const fn turn_count(&self) -> usize {
        self.turn_history.len()
    }

    /// Format a cost summary for display (e.g., for /cost command).
    #[must_use]
    pub fn format_summary(&self) -> String {
        format!(
            "Session cost: ${:.2} ({} turns)",
            self.session_cost,
            self.turn_history.len()
        )
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_pricing_calculation_correct() {
        let pricing = ModelPricing::sonnet();
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_creation_tokens: 100,
        };
        let cost = pricing.calculate(&usage);
        // input: 1000/1M * 3.0 = 0.003
        // output: 500/1M * 15.0 = 0.0075
        // cache_read: 200/1M * 0.3 = 0.00006
        // cache_creation: 100/1M * 3.75 = 0.000375
        let expected = 0.003 + 0.0075 + 0.000_06 + 0.000_375;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn different_models_have_different_pricing() {
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 1000,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let sonnet_cost = ModelPricing::sonnet().calculate(&usage);
        let opus_cost = ModelPricing::opus().calculate(&usage);
        let haiku_cost = ModelPricing::haiku().calculate(&usage);

        assert!(opus_cost > sonnet_cost);
        assert!(sonnet_cost > haiku_cost);
    }

    #[test]
    fn pricing_for_model_resolves_correctly() {
        let sonnet_p = pricing_for_model(&ModelId::sonnet());
        assert_eq!(sonnet_p.input_per_million, 3.0);

        let opus_p = pricing_for_model(&ModelId::opus());
        assert_eq!(opus_p.input_per_million, 15.0);

        let haiku_p = pricing_for_model(&ModelId::haiku());
        assert_eq!(haiku_p.input_per_million, 0.8);
    }

    #[test]
    fn pricing_for_model_defaults_to_sonnet() {
        let unknown = ModelId("unknown-model".into());
        let p = pricing_for_model(&unknown);
        assert_eq!(p.input_per_million, 3.0);
    }

    // A `mul_add` rewrite of the tolerance check below would be harder to
    // read for no measurable gain in a test.
    #[allow(clippy::suboptimal_flops)]
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
        assert!((tracker.get_total_cost() - turn1_cost * 2.0).abs() < 1e-9);
    }

    #[test]
    fn cost_tracker_session_cost_accumulates() {
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

        tracker.start_turn();
        tracker.record_usage(&usage, &ModelId::sonnet());
        tracker.end_turn(2, &usage, &ModelId::sonnet());

        assert!(tracker.get_total_cost() > 0.0);
        assert_eq!(tracker.turn_count(), 2);
    }

    #[test]
    fn cost_tracker_agent_costs() {
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
    fn custom_pricing_overrides_default() {
        let mut tracker = CostTracker::new();
        tracker.register_pricing(
            "claude-sonnet-4-6".into(),
            ModelPricing {
                input_per_million: 1.0,
                output_per_million: 1.0,
                cache_read_per_million: 0.0,
                cache_creation_per_million: 0.0,
            },
        );

        let usage = UsageSummary {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        tracker.start_turn();
        let cost = tracker.record_usage(&usage, &ModelId::sonnet());
        assert!((cost - 2.0).abs() < 1e-9); // 1.0 + 1.0 with custom pricing
    }

    #[test]
    fn load_pricing_from_valid_json() {
        let json = r#"{
            "custom-model": {
                "input_per_million": 1.0,
                "output_per_million": 2.0,
                "cache_read_per_million": 0.1,
                "cache_creation_per_million": 0.5
            }
        }"#;
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmpfile.path(), json).unwrap();
        let pricing = load_pricing_from_path(tmpfile.path());
        assert!(pricing.contains_key("custom-model"));
        let model = pricing.get("custom-model").unwrap();
        assert!((model.input_per_million - 1.0).abs() < 1e-9);
        assert!((model.output_per_million - 2.0).abs() < 1e-9);
    }

    #[test]
    fn load_pricing_missing_file_returns_empty() {
        let pricing = load_pricing_from_path(std::path::Path::new("/nonexistent/pricing.json"));
        assert!(pricing.is_empty());
    }

    #[test]
    fn load_pricing_invalid_json_returns_empty() {
        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmpfile.path(), "not json").unwrap();
        let pricing = load_pricing_from_path(tmpfile.path());
        assert!(pricing.is_empty());
    }
}
