//! Token budget tracking — continuation limits, diminishing returns detection.

use crate::types::UsageSummary;
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Configuration for token budget thresholds.
#[derive(Debug, Clone, Copy)]
pub struct BudgetThresholds {
    /// Maximum total tokens per turn.
    pub max_tokens_per_turn: u64,
    /// Maximum total tokens per session.
    pub max_tokens_per_session: u64,
    /// Continuation count threshold (warn after this many continuations).
    pub max_continuations: u32,
    /// Diminishing returns threshold: if the last delta is less than
    /// this fraction of the previous delta, consider it diminishing.
    pub diminishing_returns_ratio: f64,
}

impl Default for BudgetThresholds {
    fn default() -> Self {
        Self {
            max_tokens_per_turn: 200_000,
            max_tokens_per_session: 1_000_000,
            max_continuations: 5,
            diminishing_returns_ratio: 0.1,
        }
    }
}

/// Tracks token usage budget across turns and sessions.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Number of continuation responses in the current turn.
    pub continuation_count: u32,
    /// Tokens in the last delta response.
    pub last_delta_tokens: u64,
    /// Previous delta tokens (for diminishing returns detection).
    pub prev_delta_tokens: u64,
    /// Total tokens used in the current turn.
    pub turn_tokens: u64,
    /// Total tokens used in the session.
    pub session_tokens: u64,
    /// When tracking started.
    pub started_at: DateTime<Utc>,
    /// Thresholds for budget limits.
    pub thresholds: BudgetThresholds,
}

impl TokenBudget {
    /// Create a new token budget with the given thresholds.
    #[must_use]
    pub fn new(thresholds: BudgetThresholds) -> Self {
        Self {
            continuation_count: 0,
            last_delta_tokens: 0,
            prev_delta_tokens: 0,
            turn_tokens: 0,
            session_tokens: 0,
            started_at: Utc::now(),
            thresholds,
        }
    }

    /// Create a new token budget with default thresholds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(BudgetThresholds::default())
    }

    /// Record token usage from an API response.
    pub const fn record_tokens(&mut self, usage: &UsageSummary) {
        let total_delta = usage.output_tokens as u64;
        self.prev_delta_tokens = self.last_delta_tokens;
        self.last_delta_tokens = total_delta;
        self.turn_tokens += total_delta;
        self.session_tokens += total_delta;
    }

    /// Mark that a continuation was requested.
    pub const fn record_continuation(&mut self) {
        self.continuation_count += 1;
    }

    /// Reset the turn-level counters (called at the start of a new turn).
    pub const fn reset_turn(&mut self) {
        self.continuation_count = 0;
        self.turn_tokens = 0;
        self.last_delta_tokens = 0;
        self.prev_delta_tokens = 0;
    }

    /// Check if token usage is near the configured limit.
    ///
    /// Returns true if either turn or session usage exceeds 80% of the limit.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn is_near_limit(&self) -> bool {
        let turn_ratio = self.turn_tokens as f64 / self.thresholds.max_tokens_per_turn as f64;
        let session_ratio =
            self.session_tokens as f64 / self.thresholds.max_tokens_per_session as f64;
        turn_ratio > 0.8 || session_ratio > 0.8
    }

    /// Check if the API is showing diminishing returns.
    ///
    /// Returns true if the last delta is less than the configured ratio
    /// of the previous delta (indicating each continuation yields less content).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn is_diminishing_returns(&self) -> bool {
        if self.prev_delta_tokens == 0 || self.continuation_count == 0 {
            return false;
        }
        let ratio = self.last_delta_tokens as f64 / self.prev_delta_tokens as f64;
        ratio < self.thresholds.diminishing_returns_ratio
    }

    /// Determine if another continuation should be made.
    ///
    /// Returns false if:
    /// - Turn token limit is reached
    /// - Session token limit is reached
    /// - Maximum continuation count is exceeded
    /// - Diminishing returns detected
    /// - Usage is near limit
    #[must_use]
    pub fn should_continue(&self) -> bool {
        if self.turn_tokens >= self.thresholds.max_tokens_per_turn {
            return false;
        }
        if self.session_tokens >= self.thresholds.max_tokens_per_session {
            return false;
        }
        if self.continuation_count >= self.thresholds.max_continuations {
            return false;
        }
        if self.is_diminishing_returns() {
            return false;
        }
        if self.is_near_limit() {
            return false;
        }
        true
    }

    /// Get the elapsed time since tracking started.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        Utc::now()
            .signed_duration_since(self.started_at)
            .to_std()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_usage(output_tokens: u64) -> UsageSummary {
        UsageSummary {
            input_tokens: 0,
            output_tokens: output_tokens as usize,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    #[test]
    fn record_tokens_accumulates_correctly() {
        let mut budget = TokenBudget::with_defaults();

        budget.record_tokens(&make_usage(100));
        assert_eq!(budget.last_delta_tokens, 100);
        assert_eq!(budget.turn_tokens, 100);

        budget.record_tokens(&make_usage(200));
        assert_eq!(budget.last_delta_tokens, 200);
        assert_eq!(budget.turn_tokens, 300);
    }

    #[test]
    fn continuation_counter_increments() {
        let mut budget = TokenBudget::with_defaults();
        assert_eq!(budget.continuation_count, 0);

        budget.record_continuation();
        assert_eq!(budget.continuation_count, 1);

        budget.record_continuation();
        assert_eq!(budget.continuation_count, 2);
    }

    #[test]
    fn reset_turn_clears_counters() {
        let mut budget = TokenBudget::with_defaults();
        budget.record_tokens(&make_usage(500));
        budget.record_continuation();
        budget.record_continuation();

        budget.reset_turn();

        assert_eq!(budget.turn_tokens, 0);
        assert_eq!(budget.continuation_count, 0);
        assert_eq!(budget.last_delta_tokens, 0);
        assert_eq!(budget.prev_delta_tokens, 0);
    }

    #[test]
    fn is_near_limit_detects_80_percent() {
        let thresholds = BudgetThresholds {
            max_tokens_per_turn: 1000,
            max_tokens_per_session: 10_000,
            ..BudgetThresholds::default()
        };
        let mut budget = TokenBudget::new(thresholds);

        budget.record_tokens(&make_usage(801));
        assert!(budget.is_near_limit());
    }

    #[test]
    fn is_diminishing_returns_detects_small_delta() {
        let mut budget = TokenBudget::with_defaults();

        // First response
        budget.record_tokens(&make_usage(1000));
        budget.record_continuation();
        assert!(!budget.is_diminishing_returns());

        // Second response with tiny delta
        budget.record_tokens(&make_usage(50));
        assert!(budget.is_diminishing_returns());
    }

    #[test]
    fn is_diminishing_returns_false_on_first_response() {
        let mut budget = TokenBudget::with_defaults();
        budget.record_tokens(&make_usage(100));
        assert!(!budget.is_diminishing_returns());
    }

    #[test]
    fn should_continue_false_when_turn_limit_reached() {
        let thresholds = BudgetThresholds {
            max_tokens_per_turn: 1000,
            ..BudgetThresholds::default()
        };
        let mut budget = TokenBudget::new(thresholds);
        budget.record_tokens(&make_usage(1000));
        assert!(!budget.should_continue());
    }

    #[test]
    fn should_continue_false_when_session_limit_reached() {
        let thresholds = BudgetThresholds {
            max_tokens_per_session: 1000,
            max_tokens_per_turn: 10_000,
            ..BudgetThresholds::default()
        };
        let mut budget = TokenBudget::new(thresholds);
        budget.record_tokens(&make_usage(1000));
        assert!(!budget.should_continue());
    }

    #[test]
    fn should_continue_false_when_max_continuations_exceeded() {
        let thresholds = BudgetThresholds {
            max_continuations: 2,
            max_tokens_per_turn: 1_000_000,
            max_tokens_per_session: 10_000_000,
            ..BudgetThresholds::default()
        };
        let mut budget = TokenBudget::new(thresholds);
        budget.record_tokens(&make_usage(500));
        budget.record_continuation();
        budget.record_tokens(&make_usage(500));
        budget.record_continuation();
        // Now continuation_count = 2, should stop
        assert!(!budget.should_continue());
    }

    #[test]
    fn should_continue_false_on_diminishing_returns() {
        let mut budget = TokenBudget::with_defaults();
        budget.record_tokens(&make_usage(5000));
        budget.record_continuation();
        budget.record_tokens(&make_usage(100));
        assert!(!budget.should_continue());
    }

    #[test]
    fn should_continue_true_when_within_limits() {
        let mut budget = TokenBudget::with_defaults();
        budget.record_tokens(&make_usage(500));
        assert!(budget.should_continue());
    }
}
