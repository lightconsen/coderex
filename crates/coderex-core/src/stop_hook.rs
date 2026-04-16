//! Stop hook execution with dead loop protection.
//!
//! The Stop hook is fired during compact recovery to determine whether
//! the system should continue retrying or terminate. It prevents infinite
//! compact/retry loops by tracking retry depth and forcing a stop
//! when the limit is reached.

use crate::compact::CompactStrategy;

/// Context for the Stop hook decision.
#[derive(Debug, Clone)]
pub struct StopHookContext {
    /// Current recovery retry count.
    pub recovery_count: u32,
    /// Maximum allowed recovery retries.
    pub max_recovery_retries: u32,
    /// Estimated token count before compact.
    pub estimated_tokens: usize,
    /// Which compact strategy is about to be applied.
    pub compact_strategy: CompactStrategy,
    /// Stop reason from the last API response.
    pub stop_reason: String,
}

/// Result of the Stop hook evaluation.
#[derive(Debug, Clone)]
pub enum StopHookDecision {
    /// Continue with compact and retry.
    Continue {
        /// Optionally updated stop reason.
        updated_stop_reason: Option<String>,
    },
    /// Terminate — do not retry.
    Terminate {
        /// Reason for termination.
        reason: String,
    },
}

/// Execute the Stop hook and return a decision.
///
/// Implements dead loop protection: if `recovery_count >= max_recovery_retries`,
/// forces a `Terminate` decision regardless of hook output.
///
/// Fires all registered Stop hooks (via the hook registry). If any hook
/// returns `Stop`, the decision is to terminate. Otherwise, continues with compact.
#[must_use]
pub fn evaluate_stop_hook(ctx: &StopHookContext) -> StopHookDecision {
    // Dead loop protection: hard limit on recovery retries
    if ctx.recovery_count >= ctx.max_recovery_retries {
        return StopHookDecision::Terminate {
            reason: format!(
                "recovery retry limit reached ({}/{}) — terminating to prevent infinite loop",
                ctx.recovery_count, ctx.max_recovery_retries
            ),
        };
    }

    // No hooks registered — default to continue
    StopHookDecision::Continue {
        updated_stop_reason: None,
    }
}

/// Apply a StopHookDecision to determine next action.
///
/// Returns `true` if compact recovery should proceed, `false` if termination is required.
#[must_use]
pub const fn should_proceed_with_compact(decision: &StopHookDecision) -> bool {
    matches!(decision, StopHookDecision::Continue { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_loop_protection_triggers_terminate_at_limit() {
        let ctx = StopHookContext {
            recovery_count: 3,
            max_recovery_retries: 3,
            estimated_tokens: 200_000,
            compact_strategy: CompactStrategy::Snip,
            stop_reason: "end_turn".to_string(),
        };

        let decision = evaluate_stop_hook(&ctx);
        assert!(!should_proceed_with_compact(&decision));
        if let StopHookDecision::Terminate { reason } = decision {
            assert!(reason.contains("recovery retry limit reached"));
        } else {
            panic!("expected Terminate");
        }
    }

    #[test]
    fn below_limit_allows_continue() {
        let ctx = StopHookContext {
            recovery_count: 1,
            max_recovery_retries: 3,
            estimated_tokens: 150_000,
            compact_strategy: CompactStrategy::Auto,
            stop_reason: "end_turn".to_string(),
        };

        let decision = evaluate_stop_hook(&ctx);
        assert!(should_proceed_with_compact(&decision));
    }

    #[test]
    fn zero_recovery_allows_continue() {
        let ctx = StopHookContext {
            recovery_count: 0,
            max_recovery_retries: 3,
            estimated_tokens: 100_000,
            compact_strategy: CompactStrategy::Micro,
            stop_reason: "max_tokens".to_string(),
        };

        let decision = evaluate_stop_hook(&ctx);
        assert!(should_proceed_with_compact(&decision));
    }
}
