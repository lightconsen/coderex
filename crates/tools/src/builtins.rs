//! Built-in tools: EchoTool, NoOpTool, and all baseline tool implementations.

mod agent_tool;
mod ask_user_question_tool;
mod bash_tool;
mod brief_tool;
pub mod chrome;
mod config_tool;
mod cron_create_tool;
mod cron_delete_tool;
mod cron_list_tool;
mod current_dir_tool;
mod edit_tool;
mod enter_plan_mode_tool;
mod exit_plan_mode_tool;
mod glob_tool;
mod grep_tool;
mod list_dir_tool;
mod lsp_tool;
mod mcp_auth_tool;
mod mcp_resource_tools;
mod notebook_edit_tool;
mod powershell_tool;
mod read_tool;
mod remote_trigger_tool;
mod repl_tool;
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
pub mod team_create_tool;
mod team_delete_tool;
mod todo_write_tool;
mod tool_search_tool;
mod web_fetch_tool;
mod web_search_tool;
mod worktree_tools;
mod write_tool;

pub use agent_tool::AgentTool;
pub use ask_user_question_tool::AskUserQuestionTool;
pub use bash_tool::BashTool;
pub use brief_tool::BriefTool;
pub use chrome::{
    ChromeClient, ChromeExecuteJsTool, ChromeGetContentTool, ChromeListTabsTool,
    ChromeNavigateTool, ChromeScreenshotTool,
};
pub use config_tool::ConfigTool;
pub use cron_create_tool::CronCreateTool;
pub use cron_delete_tool::CronDeleteTool;
pub use cron_list_tool::CronListTool;
pub use current_dir_tool::CurrentDirTool;
pub use edit_tool::EditTool;
pub use enter_plan_mode_tool::EnterPlanModeTool;
pub use exit_plan_mode_tool::ExitPlanModeTool;
pub use glob_tool::GlobTool;
pub use grep_tool::GrepTool;
pub use list_dir_tool::ListDirTool;
pub use lsp_tool::LspTool;
pub use mcp_auth_tool::{
    McpAuthTool, cache_discovery_at, default_token_path, persist_token_at,
    persist_token_with_endpoints_at,
};
pub use mcp_resource_tools::{ListMcpResourcesTool, ReadMcpResourceTool};
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
pub use team_create_tool::TeamCreateTool;
pub use team_delete_tool::TeamDeleteTool;
pub use todo_write_tool::TodoWriteTool;
pub use tool_search_tool::ToolSearchTool;
pub use web_fetch_tool::WebFetchTool;
pub use web_search_tool::WebSearchTool;
pub use worktree_tools::{EnterWorktreeTool, ExitWorktreeTool};
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

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let echoed = match &input.arguments {
            Value::Object(map) => {
                if let Some(msg) = map.get("message").and_then(Value::as_str) {
                    msg.to_string()
                } else {
                    serde_json::to_string_pretty(&input.arguments).unwrap_or_else(|_| String::new())
                }
            }
            other => other.to_string(),
        };
        Ok(ToolOutput {
            output: echoed,
            is_display: true,
            images: Vec::new(),
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

    async fn execute(
        &self,
        _input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            output: String::new(),
            is_display: false,
            images: Vec::new(),
        })
    }
}

/// Whether this platform's shell is PowerShell.
///
/// There is one shell role with two platform spellings, and the platform picks
/// which: on Windows it is PowerShell, everywhere else Bash. The reference says
/// so in its own words — the REPL's `!` mode calls the Bash tool and "PowerShell
/// 平台走 PowerShellTool", the headless path uses `/bin/sh -c` and "Windows 用
/// `pwsh -NoProfile -Command`" (`subsystems/bash.md` §3.12) — and the same
/// pairing runs through its sandbox layer, which wraps "the Bash tool / the
/// PowerShell tool", and its permission classes, `["Bash","PowerShell"]`.
#[must_use]
pub const fn platform_shell_is_powershell() -> bool {
    cfg!(windows)
}

/// The name of the shell tool this platform runs commands with.
#[must_use]
pub const fn platform_shell_tool() -> &'static str {
    if platform_shell_is_powershell() {
        "PowerShell"
    } else {
        "Bash"
    }
}

/// Register the one shell tool this platform runs commands with.
///
/// Registering both, as this used to, advertised a shell the platform does not
/// use — the model could call `PowerShell` on a machine with no `pwsh`, and it
/// reached a tool that has no sandbox path wired. The non-native tool still
/// exists and still compiles; it is simply not offered where it is not the
/// shell.
pub fn register_shell_tool(reg: &mut crate::registry::ToolRegistry) {
    use std::sync::Arc;

    if platform_shell_is_powershell() {
        reg.register(Arc::new(PowerShellTool));
    } else {
        reg.register(Arc::new(BashTool::new()));
    }
}

/// Register all built-in tools into the given registry.
///
/// This registers 44 baseline tools plus 6 feature-gated tools — the 5 Chrome
/// DevTools tools and `WebFetch` — for 50 registrations in total. The platform
/// registers one shell tool, so `Bash` and `PowerShell` are alternatives rather
/// than two slots. No stub tools are registered: every tool here is one the
/// model can actually use.
pub fn register_builtin_tools(reg: &mut crate::registry::ToolRegistry) {
    use std::sync::Arc;
    reg.register(Arc::new(ReadTool));
    reg.register(Arc::new(WriteTool));
    reg.register(Arc::new(EditTool));
    reg.register(Arc::new(NotebookEditTool));
    register_shell_tool(reg);
    reg.register(Arc::new(SleepTool));
    reg.register(Arc::new(GlobTool));
    reg.register(Arc::new(GrepTool));
    reg.register(Arc::new(LspTool));
    reg.register(Arc::new(WebFetchTool::new()));
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
    reg.register(Arc::new(CurrentDirTool));
    reg.register(Arc::new(ListDirTool));
    reg.register(Arc::new(BriefTool));
    reg.register(Arc::new(ConfigTool));
    reg.register(Arc::new(SkillTool));
    reg.register(Arc::new(StructuredOutputTool));
    reg.register(Arc::new(ReplTool));
    reg.register(Arc::new(SyntheticOutputTool));
    reg.register(Arc::new(SendMessageTool));
    reg.register(Arc::new(AgentTool::new()));
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

    // Chrome DevTools tools (feature-gated)
    reg.register(Arc::new(ChromeListTabsTool));
    reg.register(Arc::new(ChromeNavigateTool));
    reg.register(Arc::new(ChromeExecuteJsTool));
    reg.register(Arc::new(ChromeScreenshotTool));
    reg.register(Arc::new(ChromeGetContentTool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(crate::context::ExecutionContext::Normal, Path::new("/tmp"))
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

    // ─── Tool Property Validation (Equivalence Test Plan §2) ───────────────

    #[test]
    fn read_tool_is_read_only() {
        let desc = ReadTool.descriptor();
        assert!(desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn write_tool_is_not_read_only() {
        let desc = WriteTool.descriptor();
        assert!(!desc.is_read_only);
        assert!(desc.is_enabled);
    }

    #[test]
    fn edit_tool_is_not_read_only() {
        let desc = EditTool.descriptor();
        assert!(!desc.is_read_only);
        assert!(desc.is_enabled);
    }

    #[test]
    fn notebook_edit_tool_is_not_read_only() {
        let desc = NotebookEditTool.descriptor();
        assert!(!desc.is_read_only);
        assert!(!desc.is_concurrency_safe);
        assert!(desc.is_enabled);
    }

    #[test]
    fn bash_tool_is_not_read_only() {
        let desc = BashTool::new().descriptor();
        assert!(!desc.is_read_only);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn glob_tool_is_read_only() {
        let desc = GlobTool.descriptor();
        assert!(desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn grep_tool_is_read_only() {
        let desc = GrepTool.descriptor();
        assert!(desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn web_fetch_tool_is_read_only() {
        let desc = WebFetchTool::new().descriptor();
        assert!(desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn web_search_tool_is_read_only() {
        let desc = WebSearchTool.descriptor();
        assert!(desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn agent_tool_is_not_read_only() {
        let desc = AgentTool::new().descriptor();
        assert!(!desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(!desc.is_concurrency_safe);
    }

    #[test]
    fn ask_user_question_is_read_only() {
        let desc = AskUserQuestionTool.descriptor();
        assert!(desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(!desc.is_concurrency_safe);
    }

    #[test]
    fn todo_write_tool_is_not_read_only() {
        let desc = TodoWriteTool.descriptor();
        assert!(!desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(!desc.is_concurrency_safe);
    }

    #[test]
    fn enter_plan_mode_is_read_only() {
        let desc = EnterPlanModeTool.descriptor();
        assert!(desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(desc.is_concurrency_safe);
    }

    #[test]
    fn enter_worktree_requires_approval() {
        let desc = EnterWorktreeTool.descriptor();
        assert!(!desc.is_read_only);
        assert!(desc.requires_approval);
        assert!(!desc.is_concurrency_safe);
    }

    #[test]
    fn cron_create_is_not_read_only() {
        let desc = CronCreateTool.descriptor();
        assert!(!desc.is_read_only);
        assert!(!desc.requires_approval);
        assert!(!desc.is_concurrency_safe);
    }

    #[test]
    fn all_tool_schemas_are_valid_json() {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ReadTool),
            Box::new(WriteTool),
            Box::new(EditTool),
            Box::new(NotebookEditTool),
            Box::new(BashTool::new()),
            Box::new(GlobTool),
            Box::new(GrepTool),
            Box::new(WebFetchTool::new()),
            Box::new(WebSearchTool),
            Box::new(AgentTool::new()),
            Box::new(AskUserQuestionTool),
            Box::new(TodoWriteTool),
            Box::new(EnterPlanModeTool),
            Box::new(EnterWorktreeTool),
            Box::new(CronCreateTool),
        ];
        for tool in &tools {
            let desc = tool.descriptor();
            assert!(
                desc.input_schema.get("type").is_some(),
                "{}: input_schema missing type field",
                desc.name
            );
            assert!(
                desc.output_schema.get("type").is_some(),
                "{}: output_schema missing type field",
                desc.name
            );
        }
    }

    #[test]
    fn all_tool_names_are_unique() {
        use std::collections::HashSet;
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ReadTool),
            Box::new(WriteTool),
            Box::new(EditTool),
            Box::new(NotebookEditTool),
            Box::new(BashTool::new()),
            Box::new(GlobTool),
            Box::new(GrepTool),
            Box::new(WebFetchTool::new()),
            Box::new(WebSearchTool),
            Box::new(AgentTool::new()),
            Box::new(AskUserQuestionTool),
            Box::new(TodoWriteTool),
            Box::new(EnterPlanModeTool),
            Box::new(EnterWorktreeTool),
            Box::new(CronCreateTool),
        ];
        let mut names = HashSet::new();
        for tool in &tools {
            let name = tool.descriptor().name;
            assert!(names.insert(name.clone()), "duplicate tool name: {name}");
        }
    }

    #[tokio::test]
    async fn tool_error_on_invalid_input() {
        // ReadTool with nonexistent file returns ToolError
        let result = ReadTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({"file": "/tmp/nonexistent_file_12345.txt"}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err(), "ReadTool should error on missing file");
    }

    // ─── The platform's shell ────────────────────────────────────────────

    /// One shell role, two platform spellings.
    #[test]
    fn the_platform_shell_is_named_for_the_platform() {
        let expected = if cfg!(windows) { "PowerShell" } else { "Bash" };
        assert_eq!(platform_shell_tool(), expected);
        assert_eq!(platform_shell_is_powershell(), cfg!(windows));
    }

    /// The advertised tool list carries the platform's shell and not the other
    /// one: a `PowerShell` tool on a machine that has no `pwsh` is an
    /// invitation to a spawn failure, and it reaches a shell with no sandbox
    /// wired.
    #[test]
    fn only_the_platforms_shell_tool_is_registered() {
        let mut registry = crate::registry::ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let names: Vec<String> = registry
            .list_tools()
            .into_iter()
            .map(|(name, _tool)| name)
            .collect();

        assert!(
            names.iter().any(|name| name == platform_shell_tool()),
            "the platform's shell tool is missing from {names:?}"
        );
        let other = if platform_shell_is_powershell() {
            "Bash"
        } else {
            "PowerShell"
        };
        assert!(
            !names.iter().any(|name| name == other),
            "the other platform's shell tool is advertised too: {names:?}"
        );
    }

    /// The registry carries 44 baseline tools and 6 feature-gated tools — the
    /// 5 Chrome DevTools tools and `WebFetch` (50 in total). Nothing is
    /// registered that the model cannot use: the former feature-gated stubs
    /// are gone, and every descriptor is enabled.
    #[test]
    fn the_registry_has_no_refusing_stubs() {
        let mut registry = crate::registry::ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let descriptors = registry.all_descriptors();

        assert_eq!(descriptors.len(), 50, "unexpected registration count");
        assert_eq!(
            descriptors
                .iter()
                .filter(|d| matches!(d.tier, ToolTier::Baseline))
                .count(),
            44,
            "unexpected baseline tool count"
        );
        assert_eq!(
            descriptors
                .iter()
                .filter(|d| matches!(d.tier, ToolTier::FeatureGated))
                .count(),
            6,
            "the feature-gated tools are the Chrome DevTools tools and WebFetch"
        );
        assert!(
            descriptors.iter().all(|d| d.is_enabled),
            "a registered tool is disabled: {:?}",
            descriptors
                .iter()
                .filter(|d| !d.is_enabled)
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// Registering the shell the platform does not use is still possible — the
    /// tool exists and compiles — but it is not something the default registry
    /// does.
    #[test]
    fn the_unused_shell_tool_still_exists() {
        let mut registry = crate::registry::ToolRegistry::new();
        registry.register(std::sync::Arc::new(PowerShellTool));
        registry.register(std::sync::Arc::new(BashTool::new()));

        let names: Vec<String> = registry
            .list_tools()
            .into_iter()
            .map(|(name, _tool)| name)
            .collect();
        assert!(names.iter().any(|name| name == "PowerShell"));
        assert!(names.iter().any(|name| name == "Bash"));
    }
}
