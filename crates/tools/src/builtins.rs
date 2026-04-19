//! Built-in tools: EchoTool, NoOpTool, and all baseline tool implementations.

mod ask_user_question_tool;
mod bash_tool;
mod brief_tool;
mod config_tool;
mod cron_create_tool;
mod cron_delete_tool;
mod cron_list_tool;
mod edit_tool;
mod enter_plan_mode_tool;
mod exit_plan_mode_tool;
mod feature_gated_tools;
mod glob_tool;
mod mcp_resource_tools;
pub mod team_create_tool;
mod team_delete_tool;
mod worktree_tools;
mod grep_tool;
mod lsp_tool;
mod mcp_auth_tool;
mod notebook_edit_tool;
mod powershell_tool;
mod read_tool;
mod remote_trigger_tool;
mod repl_tool;
mod agent_tool;
mod send_message_tool;
mod skill_tool;
mod sleep_tool;
mod structured_output_tool;
mod synthetic_output_tool;
mod task_create_tool;
mod task_get_tool;
mod task_list_tool;
mod task_output_tool;
mod task_stop_tool;
mod task_update_tool;
mod tool_search_tool;
mod todo_write_tool;
mod web_fetch_tool;
mod web_search_tool;
mod write_tool;

pub use ask_user_question_tool::AskUserQuestionTool;
pub use agent_tool::AgentTool;
pub use bash_tool::BashTool;
pub use brief_tool::BriefTool;
pub use config_tool::ConfigTool;
pub use cron_create_tool::CronCreateTool;
pub use cron_delete_tool::CronDeleteTool;
pub use cron_list_tool::CronListTool;
pub use edit_tool::EditTool;
pub use enter_plan_mode_tool::EnterPlanModeTool;
pub use exit_plan_mode_tool::ExitPlanModeTool;
pub use feature_gated_tools::register_feature_gated_tools;
pub use glob_tool::GlobTool;
pub use mcp_resource_tools::{
    ListMcpResourcesTool, ReadMcpResourceTool,
};
pub use team_create_tool::TeamCreateTool;
pub use team_delete_tool::TeamDeleteTool;
pub use worktree_tools::{EnterWorktreeTool, ExitWorktreeTool};
pub use grep_tool::GrepTool;
pub use lsp_tool::LspTool;
pub use mcp_auth_tool::McpAuthTool;
pub use notebook_edit_tool::NotebookEditTool;
pub use powershell_tool::PowerShellTool;
pub use read_tool::ReadTool;
pub use remote_trigger_tool::RemoteTriggerTool;
pub use repl_tool::ReplTool;
pub use send_message_tool::SendMessageTool;
pub use skill_tool::SkillTool;
pub use sleep_tool::SleepTool;
pub use structured_output_tool::StructuredOutputTool;
pub use synthetic_output_tool::SyntheticOutputTool;
pub use task_create_tool::TaskCreateTool;
pub use task_get_tool::TaskGetTool;
pub use task_list_tool::TaskListTool;
pub use task_output_tool::TaskOutputTool;
pub use task_stop_tool::TaskStopTool;
pub use task_update_tool::TaskUpdateTool;
pub use tool_search_tool::ToolSearchTool;
pub use todo_write_tool::TodoWriteTool;
pub use web_fetch_tool::WebFetchTool;
pub use web_search_tool::WebSearchTool;
pub use write_tool::WriteTool;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use serde_json::Value;

/// Echo tool — returns its input arguments as text.
///
/// Useful for testing and debugging the tool pipeline.
#[derive(Debug)]
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "echo".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string"}
                },
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "echoed": {"type": "string"}
                },
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let echoed = match &input.arguments {
            Value::Object(map) => {
                if let Some(msg) = map.get("message").and_then(Value::as_str) {
                    msg.to_string()
                } else {
                    serde_json::to_string_pretty(&input.arguments)
                        .unwrap_or_else(|_| String::new())
                }
            }
            other => other.to_string(),
        };
        Ok(ToolOutput {
            output: echoed,
            is_display: true,
        })
    }
}

/// No-op tool — always returns empty success.
///
/// Useful as a placeholder or for testing permission/registry logic.
#[derive(Debug)]
pub struct NoOpTool;

#[async_trait]
impl Tool for NoOpTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "noop".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, _input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: String::new(),
            is_display: false,
        })
    }
}

/// Register all baseline built-in tools into the given registry.
pub fn register_builtin_tools(reg: &mut crate::registry::ToolRegistry) {
    use std::sync::Arc;
    reg.register(Arc::new(ReadTool));
    reg.register(Arc::new(WriteTool));
    reg.register(Arc::new(EditTool));
    reg.register(Arc::new(NotebookEditTool));
    reg.register(Arc::new(BashTool));
    reg.register(Arc::new(PowerShellTool));
    reg.register(Arc::new(SleepTool));
    reg.register(Arc::new(GlobTool));
    reg.register(Arc::new(GrepTool));
    reg.register(Arc::new(LspTool));
    reg.register(Arc::new(WebFetchTool));
    reg.register(Arc::new(WebSearchTool));
    reg.register(Arc::new(TodoWriteTool));
    reg.register(Arc::new(AskUserQuestionTool));
    reg.register(Arc::new(ToolSearchTool));
    reg.register(Arc::new(TaskCreateTool));
    reg.register(Arc::new(TaskGetTool));
    reg.register(Arc::new(TaskListTool));
    reg.register(Arc::new(TaskUpdateTool));
    reg.register(Arc::new(TaskOutputTool));
    reg.register(Arc::new(TaskStopTool));
    reg.register(Arc::new(EnterPlanModeTool));
    reg.register(Arc::new(ExitPlanModeTool));
    reg.register(Arc::new(CronCreateTool));
    reg.register(Arc::new(CronDeleteTool));
    reg.register(Arc::new(CronListTool));
    reg.register(Arc::new(BriefTool));
    reg.register(Arc::new(ConfigTool));
    reg.register(Arc::new(SkillTool));
    reg.register(Arc::new(StructuredOutputTool));
    reg.register(Arc::new(ReplTool));
    reg.register(Arc::new(SyntheticOutputTool));
    reg.register(Arc::new(SendMessageTool));
    reg.register(Arc::new(AgentTool));
    reg.register(Arc::new(RemoteTriggerTool {
        bridge_url: "http://localhost:3000".into(),
    }));
    reg.register(Arc::new(McpAuthTool));
    reg.register(Arc::new(TeamCreateTool));
    reg.register(Arc::new(TeamDeleteTool));
    reg.register(Arc::new(EnterWorktreeTool));
    reg.register(Arc::new(ExitWorktreeTool));
    reg.register(Arc::new(ListMcpResourcesTool));
    reg.register(Arc::new(ReadMcpResourceTool));
    reg.register(Arc::new(EchoTool));
    reg.register(Arc::new(NoOpTool));

    // Feature-gated tools
    register_feature_gated_tools(reg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(
            crate::context::ExecutionContext::Normal,
            Path::new("/tmp"),
        )
    }

    #[tokio::test]
    async fn echo_tool_returns_input_message() {
        let output = EchoTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"message": "hello"}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "hello");
        assert!(output.is_display);
    }

    #[tokio::test]
    async fn echo_tool_falls_back_to_json_when_no_message() {
        let output = EchoTool
            .execute(
                ToolInput {
                    call_id: "2".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"key": "value"}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("key"));
        assert!(output.output.contains("value"));
    }

    #[tokio::test]
    async fn noop_tool_returns_empty_output() {
        let output = NoOpTool
            .execute(
                ToolInput {
                    call_id: "3".into(),
                    name: "noop".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.is_empty());
        assert!(!output.is_display);
    }

    #[test]
    fn echo_tool_descriptor_is_baseline() {
        let desc = EchoTool.descriptor();
        assert_eq!(desc.name, "echo");
        assert!(matches!(desc.tier, ToolTier::Baseline));
        assert!(!desc.is_read_only);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn noop_tool_descriptor_is_read_only() {
        let desc = NoOpTool.descriptor();
        assert_eq!(desc.name, "noop");
        assert!(desc.is_read_only);
        assert!(desc.is_concurrency_safe);
    }
}
