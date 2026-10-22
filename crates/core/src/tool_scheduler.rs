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
use crate::hook_events;
use crate::hook_executor::HookExecutor;
use crate::permission_engine::PermissionEngine;
use crate::permissions::PermissionResult;
use crate::tool_executor::{ToolDispatchOutcome, ToolExecutor};
use crate::types::{EngineEvent, ToolCall, ToolResult};
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
    /// Calls that need an elevation decision, in their original order.
    pub pending_elevations: Vec<crate::engine::PendingElevation>,
}

/// Execute pending tool calls with permission checks and hooks.
pub async fn run_tool_calls<S: std::hash::BuildHasher + Sync>(
    calls: &[PendingToolCall],
    tools: &dyn ToolExecutor,
    permission_engine: &mut PermissionEngine,
    hooks: &dyn HookExecutor,
    options: &SubmitOptions,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
    denied_tools: &mut Vec<DeniedToolCall>,
    summaries: &std::collections::HashMap<String, String, S>,
) -> Result<SchedulerOutcome, String> {
    let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
    let mut approved: Vec<(usize, ApprovedCall)> = Vec::new();
    let mut pending_elevations = Vec::new();

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
                    images: Vec::new(),
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
            // `tool_use_summaries` supplies a model-written label; without
            // it (or when summarization failed) use the deterministic one.
            summary: summaries
                .get(&call.id)
                .cloned()
                .unwrap_or_else(|| ToolCall::summarize_activity(&call.name, &call.input)),
        });

        // 3. PreToolUse hook
        let mut ctx = HashMap::new();
        ctx.insert("tool_call_id".to_string(), call.id.clone());
        ctx.insert("tool_input".to_string(), call.input.to_string());
        let pre_hook = hooks
            .fire_hook(hook_events::PRE_TOOL_USE, &call.name, &ctx)
            .await;
        if pre_hook.stop {
            denied_tools.push(DeniedToolCall {
                name: call.name.clone(),
                reason: "pre-use hook stopped execution".to_string(),
            });
            let result = ToolResult {
                tool_call_id: call.id.clone(),
                output: "tool execution stopped by hook".to_string(),
                is_error: true,
                images: Vec::new(),
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
                permission_context_for(options.agent_id.as_deref(), &call.id),
            )
            .await
            .map_err(|e| format!("permission check failed: {e}"))?;

        match perm_result {
            PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
                approved.push((
                    idx,
                    ApprovedCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: call.input.clone(),
                    },
                ));
            }
            PermissionResult::Ask { reason } => {
                let _ = event_tx.send(EngineEvent::PermissionRequest {
                    id: call.id.clone(),
                    tool: call.name.clone(),
                    args: call.input.clone(),
                });
                let mut ctx = HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let ask_result = hooks
                    .fire_hook(hook_events::PERMISSION_REQUEST, &call.name, &ctx)
                    .await;
                if !ask_result.errors.is_empty() {
                    tracing::warn!(
                        "PermissionRequest hook errors for {}: {:?}",
                        call.name,
                        ask_result.errors
                    );
                }
                denied_tools.push(DeniedToolCall {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: crate::permissions::PermissionResult::unapproved_tool_message(
                        &call.name, &reason,
                    ),
                    is_error: true,
                    images: Vec::new(),
                };
                let mut ctx = HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let fail_result = hooks
                    .fire_hook(hook_events::POST_TOOL_USE_FAILURE, &call.name, &ctx)
                    .await;
                if !fail_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUseFailure hook errors for {}: {:?}",
                        call.name,
                        fail_result.errors
                    );
                }
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
                // The reason is the only thing that tells the model (and the
                // person reading the transcript) what to do differently.
                let result = ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' denied: {reason}", call.name),
                    is_error: true,
                    images: Vec::new(),
                };
                let mut ctx = HashMap::new();
                ctx.insert("tool_call_id".to_string(), call.id.clone());
                ctx.insert("reason".to_string(), reason.clone());
                let denied_result = hooks
                    .fire_hook(hook_events::PERMISSION_DENIED, &call.name, &ctx)
                    .await;
                if !denied_result.errors.is_empty() {
                    tracing::warn!(
                        "PermissionDenied hook errors for {}: {:?}",
                        call.name,
                        denied_result.errors
                    );
                }
                let fail_result = hooks
                    .fire_hook(hook_events::POST_TOOL_USE_FAILURE, &call.name, &ctx)
                    .await;
                if !fail_result.errors.is_empty() {
                    tracing::warn!(
                        "PostToolUseFailure hook errors for {}: {:?}",
                        call.name,
                        fail_result.errors
                    );
                }
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
                pending_elevations.push(crate::engine::PendingElevation {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    request_id: elevation_id,
                    reason,
                });
                // Keep a result slot so the outcome stays aligned with `calls`,
                // but do not emit ToolCallEnd yet: the engine replaces this
                // placeholder with the result of the user's elevation decision.
                results[idx] = Some(ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("tool '{}' requires elevated permissions", call.name),
                    is_error: true,
                    images: Vec::new(),
                });
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
    for ((idx, call), outcome) in approved.iter().zip(outcomes) {
        let mut result = outcome.result;

        let mut ctx = HashMap::new();
        ctx.insert("tool_call_id".to_string(), call.id.clone());
        ctx.insert(
            "tool_output_len".to_string(),
            result.output.len().to_string(),
        );
        let post_result = hooks
            .fire_hook(hook_events::POST_TOOL_USE, &call.name, &ctx)
            .await;
        if !post_result.errors.is_empty() {
            tracing::warn!(
                "PostToolUse hook errors for {}: {:?}",
                call.name,
                post_result.errors
            );
        }
        // A PostToolUse hook that returned `updatedToolOutput` rewrites what
        // the model sees: that is the hook's decision power, not just a veto.
        if let Some(updated) = post_result.updated_tool_output {
            result.output = updated;
        }

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
        pending_elevations,
    })
}

/// The context a permission decision is made in.
///
/// `BackgroundAgentDenyHook` reads `execution_mode` to refuse tools in a
/// background run, and every call site passed an empty map — a hook that could
/// never fire. The mode comes from the submit options: a turn with an agent id
/// is a forked or background flow, and one without is the person's own.
pub(crate) fn permission_context_for(
    agent_id: Option<&str>,
    tool_call_id: &str,
) -> HashMap<String, String> {
    let mut context = HashMap::new();
    context.insert(
        "execution_mode".to_string(),
        if agent_id.is_some() {
            "background".to_string()
        } else {
            "interactive".to_string()
        },
    );
    context.insert("tool_call_id".to_string(), tool_call_id.to_string());
    if let Some(agent_id) = agent_id {
        context.insert("agent_id".to_string(), agent_id.to_string());
    }
    context
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
                    images: Vec::new(),
                },
            }
        }
    }

    /// Records the events fired through it, so a test can assert that a
    /// lifecycle point actually reaches the hook system.
    #[derive(Default)]
    struct RecordingHooks {
        fired: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingHooks {
        fn names(&self) -> Vec<String> {
            self.fired.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HookExecutor for RecordingHooks {
        async fn fire_hook(
            &self,
            event_name: &str,
            _input: &str,
            _context: &HashMap<String, String>,
        ) -> crate::hook_executor::HookFireResult {
            self.fired.lock().unwrap().push(event_name.to_string());
            crate::hook_executor::HookFireResult::default()
        }
    }

    /// A `PostToolUse` hook that rewrites what the model sees.
    struct RewritingHooks;

    #[async_trait::async_trait]
    impl HookExecutor for RewritingHooks {
        async fn fire_hook(
            &self,
            event_name: &str,
            _input: &str,
            _context: &HashMap<String, String>,
        ) -> crate::hook_executor::HookFireResult {
            if event_name == hook_events::POST_TOOL_USE {
                return crate::hook_executor::HookFireResult {
                    updated_tool_output: Some("redacted by hook".to_string()),
                    ..Default::default()
                };
            }
            crate::hook_executor::HookFireResult::default()
        }
    }

    /// A PostToolUse hook's `updatedToolOutput` replaces the result the model
    /// sees — the hook's decision power, not just a veto.
    #[tokio::test]
    async fn a_post_tool_use_hook_can_replace_the_tool_output() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tools = MockToolExecutor;
        let mut perm_engine = PermissionEngine::new(crate::permissions::PermissionChecker::new(
            crate::permissions::PermissionMode::BypassPermissions,
        ));
        let mut denied = Vec::new();
        let calls = vec![PendingToolCall {
            id: "t1".into(),
            name: "Read".into(),
            input: serde_json::Value::Null,
        }];

        let outcome = run_tool_calls(
            &calls,
            &tools,
            &mut perm_engine,
            &RewritingHooks,
            &SubmitOptions::default(),
            &tx,
            &mut denied,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.results[0].output, "redacted by hook");
    }

    #[tokio::test]
    async fn a_tool_waiting_for_a_decision_fires_the_permission_hook() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tools = MockToolExecutor;
        let hooks = RecordingHooks::default();
        // Default mode asks about a shell command, and no confirmation bus is
        // wired here, so the call is reported as unapproved.
        let mut perm_engine = PermissionEngine::new(crate::permissions::PermissionChecker::new(
            crate::permissions::PermissionMode::Default,
        ));
        let mut denied = Vec::new();
        let calls = vec![PendingToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }];

        let outcome = run_tool_calls(
            &calls,
            &tools,
            &mut perm_engine,
            &hooks,
            &SubmitOptions::default(),
            &tx,
            &mut denied,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.results.len(), 1);
        assert!(hooks.names().contains(&"PermissionRequest".to_string()));
    }

    #[tokio::test]
    async fn a_refused_tool_fires_the_permission_denied_hook() {
        use crate::permissions::{
            PermissionAction, PermissionChecker, PermissionMode, PermissionRule,
        };

        let (tx, _rx) = mpsc::unbounded_channel();
        let tools = MockToolExecutor;
        let hooks = RecordingHooks::default();
        let mut checker = PermissionChecker::new(PermissionMode::Default);
        checker
            .add_rule(PermissionRule {
                tool_name: "Write".into(),
                action: PermissionAction::Deny,
                paths: Vec::new(),
                description: "denied by policy".into(),
            })
            .unwrap();
        let mut perm_engine = PermissionEngine::new(checker);
        let mut denied = Vec::new();
        let calls = vec![PendingToolCall {
            id: "t1".into(),
            name: "Write".into(),
            input: serde_json::Value::Null,
        }];

        let outcome = run_tool_calls(
            &calls,
            &tools,
            &mut perm_engine,
            &hooks,
            &SubmitOptions::default(),
            &tx,
            &mut denied,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        assert!(outcome.results[0].is_error);
        assert_eq!(denied.len(), 1);
        let fired = hooks.names();
        assert!(fired.contains(&"PermissionDenied".to_string()), "{fired:?}");
        assert!(
            !fired.contains(&"PermissionRequest".to_string()),
            "a refusal is not a request: {fired:?}"
        );
    }

    #[tokio::test]
    async fn parallel_execution_runs_all_tools() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tools = MockToolExecutor;
        let hooks = crate::hook_executor::NoOpHookExecutor;
        let mut perm_engine = PermissionEngine::new(crate::permissions::PermissionChecker::new(
            crate::permissions::PermissionMode::BypassPermissions,
        ));
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
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results[0].output, "executed read");
        assert_eq!(outcome.results[1].output, "executed read");
        assert!(outcome.pending_elevations.is_empty());

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
        let mut perm_engine = PermissionEngine::new(crate::permissions::PermissionChecker::new(
            crate::permissions::PermissionMode::BypassPermissions,
        ));
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
            &calls,
            &tools,
            &mut perm_engine,
            &hooks,
            &options,
            &tx,
            &mut denied,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.results.len(), 2, "expected 2 results");
        assert!(
            !outcome.results[0].is_error,
            "read should be allowed, got: {}",
            outcome.results[0].output
        ); // read allowed
        assert!(
            outcome.results[1].is_error,
            "write should be blocked, got: {}",
            outcome.results[1].output
        ); // write blocked
        assert_eq!(denied.len(), 1);
    }
}
