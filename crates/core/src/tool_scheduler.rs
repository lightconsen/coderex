//! Parallel tool scheduler — formalizes parallel-tool dispatch inside TurnState.
//!
//! Inspired by gemini-cli's state-manager + scheduler pattern.  The engine's
//! `execute_tool_calls` delegates to this module so that:
//!
//! 1. Hooks and permission checks run **sequentially** (deterministic, avoids
//!    `&mut self` contention on `PermissionEngine`).
//! 2. The actual tool execution runs **concurrently** via `join_all` — the
//!    underlying `ToolExecutor` (e.g. `RegistryToolDispatcher`) serialises
//!    non-safe tools automatically via per-tool-name locks.
//!
//! This gives us the best of both worlds: correct ordering for policy checks
//! and maximum throughput for safe tools.

use crate::engine::{DeniedToolCall, PendingToolCall, SubmitOptions};
use crate::hook_executor::HookExecutor;
use crate::permission_engine::PermissionEngine;
use crate::permissions::PermissionResult;
use crate::tool_executor::{ToolDispatchOutcome, ToolExecutor};
use crate::types::{EngineEvent, ToolResult};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// A tool call that has passed validation and is ready for concurrent execution.
struct ApprovedCall {
    id: String,
    name: String,
    input: serde_json::Value,
}

/// Run all pending tool calls through validation and execute approved calls
/// in parallel.
///
/// The lifecycle per call is:
///
/// ```text
/// 1. allowed_tools_override check (fast, synchronous)
/// 2. emit ToolCallStart event
/// 3. PreToolUse hook  (sequential — &mut PermissionEngine)
/// 4. PermissionEngine::evaluate  (sequential — &mut self)
/// 5a. Allowed  → add to parallel execution batch
/// 5b. Denied   → emit ToolCallEnd, push to denied_tools
/// 6. Execute batch concurrently via join_all
/// 7. PostToolUse hook + ToolCallEnd event per completed call
/// ```
/// Outcome of running the tool scheduler.
#[derive(Debug)]
pub struct SchedulerOutcome {
    /// Results for all calls (approved, denied, blocked, etc.).
    pub results: Vec<ToolResult>,
    /// If any tool required elevation, the last one is captured here.
    pub pending_elevation: Option<crate::engine::PendingElevation>,
}

/// Execute pending tool calls with permission checks and hooks.
pub async fn run_tool_calls(
    calls: &[PendingToolCall],
    tools: &dyn ToolExecutor,
    permission_engine: &mut PermissionEngine,
    hooks: &dyn HookExecutor,
    options: &SubmitOptions,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
    denied_tools: &mut Vec<DeniedToolCall>,
) -> Result<SchedulerOutcome, String> {
    let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
    let mut approved: Vec<(usize, ApprovedCall)> = Vec::new();
    let mut pending_elevation: Option<crate::engine::PendingElevation> = None;

    // ─── Phase 1: Sequential validation ────────────────────────────────────
    for (idx, call) in calls.iter().enumerate() {
        // 1. allowed_tools_override
        if let Some(ref allowed) = options.allowed_tools_override {
            if !allowed.iter().any(|t| t.eq_ignore_ascii_case(&call.name)) {
                denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: "tool is not in the allowed tools override".to_string(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!(
                        "tool '{}' blocked: not in allowed tools override",
                        call.name
                    ),
                    is_error: true,
                };
                let _ = event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                results[idx] = Some(result);
                continue;
            }
        }

        // 2. ToolCallStart event
        let _ = event_tx.send(EngineEvent::ToolCallStart {
            id: call.id.clone(),
            name: call.name.clone(),
        });

        // 3. PreToolUse hook
        let mut ctx = HashMap::new();
        ctx.insert("tool_call_id".to_string(), call.id.clone());
        ctx.insert("tool_input".to_string(), call.input.to_string());
        let pre_hook = hooks.fire_hook("PreToolUse", &call.name, &ctx).await;
        if pre_hook.stop {
            denied_tools.push(DeniedToolCall {
                name: call.name.clone(),
                reason: "pre-use hook stopped execution".to_string(),
            });
            let result = ToolResult {
                tool_call_id: call.id.clone(),
                output: "tool execution stopped by hook".to_string(),
                is_error: true,
            };
            let _ = event_tx.send(EngineEvent::ToolCallEnd {
                id: call.id.clone(),
                result: result.clone(),
            });
            results[idx] = Some(result);
            continue;
        }

        // 4. Permission check
        let (_request, perm_result) = permission_engine
            .evaluate(
                &call.name,
                call.input.to_string(),
                None,
                HashMap::new(),
            )
            .await
            .map_err(|e| format!("permission check failed: {e}"))?;

        match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                approved.push((idx, ApprovedCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                }));
            }
            PermissionResult::Ask { reason } | PermissionResult::AskForSession { reason } => {
                let _ = event_tx.send(EngineEvent::PermissionRequest {
                    id: call.id.clone(),
                    tool: call.name.clone(),
                    args: call.input.clone(),
                });
                denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' denied by user", call.name),
                    is_error: true,
                };
                let mut ctx = HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let _ = hooks.fire_hook("PostToolUseFail", &call.name, &ctx).await;
                let _ = event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                results[idx] = Some(result);
            }
            PermissionResult::Deny { reason } => {
                denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' denied", call.name),
                    is_error: true,
                };
                let mut ctx = HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let _ = hooks.fire_hook("PostToolUseFail", &call.name, &ctx).await;
                let _ = event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                results[idx] = Some(result);
            }
            PermissionResult::Elevated { reason } => {
                let elevation_id = format!("elev_{}", call.id);
                let _ = event_tx.send(EngineEvent::ElevationRequest {
                    id: elevation_id.clone(),
                    tool: call.name.clone(),
                    reason: reason.clone(),
                });
                pending_elevation = Some(crate::engine::PendingElevation {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    request_id: elevation_id,
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' requires elevated permissions", call.name),
                    is_error: true,
                };
                let _ = event_tx.send(EngineEvent::ToolCallEnd {
                    id: call.id.clone(),
                    result: result.clone(),
                });
                results[idx] = Some(result);
            }
        }
    }

    // ─── Phase 2: Parallel execution ───────────────────────────────────────
    let exec_futures: Vec<_> = approved
        .iter()
        .map(|(_, c)| tools.execute(&c.id, &c.name, &c.input))
        .collect();

    let outcomes: Vec<ToolDispatchOutcome> = futures::future::join_all(exec_futures).await;

    // ─── Phase 3: Post-execution hooks & events ────────────────────────────
    for ((idx, call), outcome) in approved.iter().zip(outcomes.into_iter()) {
        let result = outcome.result;

        let mut ctx = HashMap::new();
        ctx.insert("tool_call_id".to_string(), call.id.clone());
        ctx.insert("tool_output_len".to_string(), result.output.len().to_string());
        let _ = hooks.fire_hook("PostToolUse", &call.name, &ctx).await;

        let _ = event_tx.send(EngineEvent::ToolCallEnd {
            id: call.id.clone(),
            result: result.clone(),
        });
        results[*idx] = Some(result);
    }

    // Unwrap all results (every slot should be filled)
    let results: Vec<ToolResult> = results.into_iter().flatten().collect();

    Ok(SchedulerOutcome {
        results,
        pending_elevation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    
    use tokio::sync::mpsc;

    struct MockToolExecutor;

    #[async_trait::async_trait]
    impl ToolExecutor for MockToolExecutor {
        async fn execute(
            &self,
            tool_call_id: &str,
            tool_name: &str,
            _input: &serde_json::Value,
        ) -> ToolDispatchOutcome {
            ToolDispatchOutcome {
                result: ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    output: format!("executed {tool_name}"),
                    is_error: false,
                },
            }
        }
    }

    #[tokio::test]
    async fn parallel_execution_runs_all_tools() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tools = MockToolExecutor;
        let hooks = crate::hook_executor::NoOpHookExecutor;
        let mut perm_engine = PermissionEngine::new(
            crate::permissions::PermissionChecker::new(crate::permissions::PermissionMode::BypassPermissions),
        );
        let mut denied = Vec::new();
        let options = SubmitOptions::default();

        let calls = vec![
            PendingToolCall {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"file": "a.txt"}),
            },
            PendingToolCall {
                id: "t2".into(),
                name: "read".into(),
                input: serde_json::json!({"file": "b.txt"}),
            },
        ];

        let outcome = run_tool_calls(
            &calls,
            &tools,
            &mut perm_engine,
            &hooks,
            &options,
            &tx,
            &mut denied,
        )
        .await
        .unwrap();

        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results[0].output, "executed read");
        assert_eq!(outcome.results[1].output, "executed read");
        assert!(outcome.pending_elevation.is_none());

        // Collect all events
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert_eq!(events.len(), 4); // 2× Start + 2× End
    }

    #[tokio::test]
    async fn allowed_tools_override_blocks_unlisted() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tools = MockToolExecutor;
        let hooks = crate::hook_executor::NoOpHookExecutor;
        let mut perm_engine = PermissionEngine::new(
            crate::permissions::PermissionChecker::new(crate::permissions::PermissionMode::BypassPermissions),
        );
        let mut denied = Vec::new();
        let mut options = SubmitOptions::default();
        options.allowed_tools_override = Some(vec!["read".into()]);

        let calls = vec![
            PendingToolCall {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::Value::Null,
            },
            PendingToolCall {
                id: "t2".into(),
                name: "write".into(),
                input: serde_json::Value::Null,
            },
        ];

        let outcome = run_tool_calls(
            &calls, &tools, &mut perm_engine, &hooks, &options, &tx, &mut denied,
        )
        .await
        .unwrap();

        assert_eq!(outcome.results.len(), 2, "expected 2 results");
        assert!(!outcome.results[0].is_error, "read should be allowed, got: {}", outcome.results[0].output); // read allowed
        assert!(outcome.results[1].is_error, "write should be blocked, got: {}", outcome.results[1].output);  // write blocked
        assert_eq!(denied.len(), 1);
    }
}
