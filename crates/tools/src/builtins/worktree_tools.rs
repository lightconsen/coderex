//! EnterWorktree / ExitWorktree baseline tools.
//!
//! These tools drive an injected `WorktreeHandle` without depending on
//! `coderex-core` directly (to avoid a dependency cycle).

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Shared-state key for an injected `WorktreeHandle`.
pub const WORKTREE_HANDLE_KEY: &str = "worktree_handle";

/// Opaque handle interface for worktree operations.
pub trait WorktreeHandle: Send + Sync {
    /// Enter (create/activate) a worktree. Returns the worktree path.
    fn enter(&self, branch: Option<&str>) -> Result<String, String>;
    /// Exit (remove) the currently active worktree.
    fn exit(&self) -> Result<(), String>;
}

fn get_handle(ctx: &ToolContext<'_>) -> Result<std::sync::Arc<dyn WorktreeHandle>, ToolError> {
    ctx.extra
        .get(WORKTREE_HANDLE_KEY)
        .and_then(|v| v.downcast_ref::<std::sync::Arc<dyn WorktreeHandle>>())
        .cloned()
        .ok_or_else(|| ToolError::InvalidInput {
            message: "worktree handle not available in this context".into(),
        })
}

/// Enter a git worktree for isolated work.
#[derive(Debug)]
pub struct EnterWorktreeTool;

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "EnterWorktree".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "description": "Optional branch name for the worktree" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
            requires_approval: true,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let branch = input.arguments.get("branch").and_then(serde_json::Value::as_str);
        let handle = get_handle(&ctx)?;
        let path = handle
            .enter(branch)
            .map_err(|e| ToolError::InvalidInput { message: e })?;
        Ok(ToolOutput { output: path, is_display: true })
    }
}

/// Exit the current git worktree.
#[derive(Debug)]
pub struct ExitWorktreeTool;

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ExitWorktree".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "exited": { "type": "boolean" } }
            }),
            requires_approval: true,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, _input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let handle = get_handle(&ctx)?;
        handle.exit().map_err(|e| ToolError::InvalidInput { message: e })?;
        Ok(ToolOutput { output: "exited".into(), is_display: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    struct StubHandle(Mutex<Option<String>>);
    impl WorktreeHandle for StubHandle {
        fn enter(&self, branch: Option<&str>) -> Result<String, String> {
            let name = branch.unwrap_or("feat").to_string();
            *self.0.lock().unwrap() = Some(name.clone());
            Ok(format!("/tmp/wt/{name}"))
        }
        fn exit(&self) -> Result<(), String> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    fn ctx_with_handle() -> (ToolContext<'static>, Arc<StubHandle>) {
        let h = Arc::new(StubHandle(Mutex::new(None)));
        let typed: Arc<dyn WorktreeHandle> = h.clone();
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(WORKTREE_HANDLE_KEY.into(), Box::new(typed));
        (
            ToolContext {
            elicit_tx: None,
                execution_context: ExecutionContext::Normal,
                cwd: Path::new("/tmp"),
                extra: Arc::new(extra),
            },
            h,
        )
    }

    #[tokio::test]
    async fn enter_worktree_returns_path() {
        let (ctx, _h) = ctx_with_handle();
        let out = EnterWorktreeTool
            .execute(
                ToolInput { call_id: "1".into(), name: "EnterWorktree".into(), arguments: serde_json::json!({"branch": "dev"}) },
                ctx,
            )
            .await
            .unwrap();
        assert!(out.output.contains("dev"));
    }

    #[tokio::test]
    async fn exit_worktree_succeeds() {
        let (ctx, _h) = ctx_with_handle();
        let out = ExitWorktreeTool
            .execute(
                ToolInput { call_id: "1".into(), name: "ExitWorktree".into(), arguments: serde_json::json!({}) },
                ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.output, "exited");
    }

    #[tokio::test]
    async fn missing_handle_errors() {
        let ctx = ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"));
        let err = EnterWorktreeTool
            .execute(
                ToolInput { call_id: "1".into(), name: "EnterWorktree".into(), arguments: serde_json::json!({}) },
                ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }
}
