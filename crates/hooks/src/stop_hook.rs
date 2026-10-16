//! Stop hook execution with dead loop protection.
//!
//! The Stop hook is fired during compact recovery to determine whether
//! the system should continue retrying or terminate. It prevents infinite
//! compact/retry loops by tracking retry depth and forcing a stop
//! when the limit is reached.

use crate::engine::{HookInput, HookRegistry};
use crate::events::{HookAction, HookEvent};

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
    pub compact_strategy: String,
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
/// Fires all registered Stop hooks. If any hook returns `Stop`,
/// the decision is to terminate. Otherwise, continues with compact.
///
/// Also implements dead loop protection: if `recovery_count >= max_recovery_retries`,
/// forces a `Terminate` decision regardless of hook output.
pub async fn execute_stop_hook(registry: &HookRegistry, ctx: &StopHookContext) -> StopHookDecision {
    // Dead loop protection: hard limit on recovery retries
    if ctx.recovery_count >= ctx.max_recovery_retries {
        return StopHookDecision::Terminate {
            reason: format!(
                "recovery retry limit reached ({}/{}) — terminating to prevent infinite loop",
                ctx.recovery_count, ctx.max_recovery_retries
            ),
        };
    }

    // Build hook input
    let input = HookInput {
        event: "Stop".to_string(),
        session_id: None,
        input: Some(format!(
            "Compact recovery #{} of {}. Stop reason: {}. Strategy: {}. Tokens: {}.",
            ctx.recovery_count + 1,
            ctx.max_recovery_retries,
            ctx.stop_reason,
            ctx.compact_strategy,
            ctx.estimated_tokens,
        )),
        context: None,
    };

    // Execute Stop hooks
    let results = registry.execute_async(&HookEvent::Stop, &input).await;

    for exec_result in &results {
        match exec_result {
            // A hook that returns Stop, terminates.
            Ok(exec_result) if matches!(exec_result.hook_result.action, HookAction::Stop) => {
                let reason = exec_result
                    .hook_result
                    .additional_context
                    .clone()
                    .unwrap_or_else(|| "stop hook requested termination".to_string());
                return StopHookDecision::Terminate { reason };
            }
            Ok(_) => {}
            // Fail closed: a Stop hook that cannot answer must not be read as
            // "continue" — that would let a broken hook wave through the
            // compact/retry it was meant to gate.
            Err(e) => {
                return StopHookDecision::Terminate {
                    reason: format!("stop hook could not answer: {e}"),
                };
            }
        }
    }

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

    #[tokio::test]
    async fn dead_loop_protection_triggers_terminate_at_limit() {
        let registry = HookRegistry::new();
        let ctx = StopHookContext {
            recovery_count: 3,
            max_recovery_retries: 3,
            estimated_tokens: 200_000,
            compact_strategy: "Snip".to_string(),
            stop_reason: "end_turn".to_string(),
        };

        let decision = execute_stop_hook(&registry, &ctx).await;
        assert!(!should_proceed_with_compact(&decision));
        if let StopHookDecision::Terminate { reason } = decision {
            assert!(reason.contains("recovery retry limit reached"));
        } else {
            panic!("expected Terminate");
        }
    }

    /// A Stop hook that cannot answer must terminate, not silently continue.
    #[tokio::test]
    async fn failing_stop_hook_terminates() {
        use crate::engine::{HookConfig, HookTier, HookType};
        use std::collections::HashMap;

        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "Stop".to_string(),
            kind: HookType::Command,
            command: "exit 1".to_string(),
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
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            tier: HookTier::Core,
        });
        let ctx = StopHookContext {
            recovery_count: 1,
            max_recovery_retries: 3,
            estimated_tokens: 150_000,
            compact_strategy: "Auto".to_string(),
            stop_reason: "end_turn".to_string(),
        };

        let decision = execute_stop_hook(&registry, &ctx).await;
        assert!(!should_proceed_with_compact(&decision));
        if let StopHookDecision::Terminate { reason } = decision {
            assert!(reason.contains("could not answer"));
        } else {
            panic!("expected Terminate");
        }
    }

    #[tokio::test]
    async fn below_limit_allows_continue() {
        let registry = HookRegistry::new();
        let ctx = StopHookContext {
            recovery_count: 1,
            max_recovery_retries: 3,
            estimated_tokens: 150_000,
            compact_strategy: "Auto".to_string(),
            stop_reason: "end_turn".to_string(),
        };

        let decision = execute_stop_hook(&registry, &ctx).await;
        assert!(should_proceed_with_compact(&decision));
    }

    #[tokio::test]
    async fn zero_recovery_allows_continue() {
        let registry = HookRegistry::new();
        let ctx = StopHookContext {
            recovery_count: 0,
            max_recovery_retries: 3,
            estimated_tokens: 100_000,
            compact_strategy: "Micro".to_string(),
            stop_reason: "max_tokens".to_string(),
        };

        let decision = execute_stop_hook(&registry, &ctx).await;
        assert!(should_proceed_with_compact(&decision));
    }
}
