//! Agent tool — spawn a sub-agent via AgentManager.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{
    Backgrounding, Tool, ToolCapabilities, ToolContext, ToolDescriptor, ToolInput, ToolTier,
};
use async_trait::async_trait;
use std::sync::Arc;

/// Spawn a sub-agent running coderex with the given prompt.
#[derive(Debug)]
pub struct AgentTool {
    manager: Option<Arc<tokio::sync::Mutex<coderex_agents::manager::AgentManager>>>,
    session_id: String,
    cwd: String,
}

impl Default for AgentTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTool {
    /// Create a new AgentTool without a manager; execution reports that delegation is unavailable.
    #[must_use]
    pub fn new() -> Self {
        Self {
            manager: None,
            session_id: "default".into(),
            cwd: ".".into(),
        }
    }

    /// Attach an AgentManager so the tool can spawn real agents.
    #[must_use]
    pub fn with_manager(
        mut self,
        manager: Arc<tokio::sync::Mutex<coderex_agents::manager::AgentManager>>,
    ) -> Self {
        self.manager = Some(manager);
        self
    }

    /// Set the session ID used when spawning agents.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Set the working directory used when spawning agents.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = cwd.into();
        self
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Agent".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["description", "prompt"],
                "properties": {
                    "description": { "type": "string" },
                    "prompt": { "type": "string" },
                    "subagent_type": { "type": "string" },
                    "model": { "type": "string" },
                    "run_in_background": { "type": "boolean" },
                    "isolation": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "output_file": { "type": "string" },
                    "output": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            // `run_in_background` is read below and really does return early
            // with an agent handle, so the schema's promise is kept.
            backgrounding: Backgrounding::SelfManaged,
            // No rule exemptions. Delegation is not a "runner" in the sense the
            // reference means (that is a CI self-hosted runner, which re-runs a
            // session somewhere else): this tool self-allows in the ordinary
            // modes, and the sub-agent's own tool calls are gated by the same
            // checker anyway. Declaring them here would refuse a whole-tool
            // allow rule the user wrote on purpose, for no security gain.
            ..ToolCapabilities::default()
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let description = extract_string(&input.arguments, "description")?;
        let prompt = extract_string(&input.arguments, "prompt")?;
        let background = input
            .arguments
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let subagent_type = input
            .arguments
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("general_purpose");
        let model_override = input
            .arguments
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from);
        let isolation = input
            .arguments
            .get("isolation")
            .and_then(|v| v.as_str())
            .map(String::from);

        if let Some(ref manager) = self.manager {
            use coderex_agents::manager::SpawnOptions;
            use coderex_agents::types::AgentType;

            let agent_type = match subagent_type {
                "explore" | "research" => AgentType::Explore {
                    thoroughness: coderex_agents::types::Thoroughness::Medium,
                },
                "plan" => AgentType::Plan,
                "verification" | "verify" => AgentType::Verification,
                "claude_code_guide" => AgentType::ClaudeCodeGuide,
                "statusline_setup" => AgentType::StatuslineSetup,
                _ => AgentType::GeneralPurpose,
            };

            let options = SpawnOptions {
                run_in_background: background,
                model_override,
                isolation,
                ..Default::default()
            };

            let mut mgr = manager.lock().await;
            let instance = mgr
                .spawn_agent(agent_type, &prompt, options, &self.session_id, &self.cwd, 0)
                .await
                .map_err(|e| ToolError::InvalidInput {
                    message: format!("failed to spawn agent: {e}"),
                })?;

            let agent_id = instance.task_handle.id.clone();
            let output_file = instance.output_file.clone();

            if background {
                return Ok(ToolOutput {
                    output: serde_json::json!({
                        "agent_id": agent_id,
                        "output_file": output_file,
                        "note": "agent running in background"
                    })
                    .to_string(),
                    is_display: true,
                });
            }

            // Synchronous: wait for completion and return the agent's output
            let result =
                mgr.wait_for_completion(&agent_id)
                    .await
                    .map_err(|e| ToolError::InvalidInput {
                        message: format!("agent execution failed: {e}"),
                    })?;

            return Ok(ToolOutput {
                output: serde_json::json!({
                    "agent_id": agent_id,
                    "output_file": output_file,
                    "output": result.output,
                    "status": format!("{:?}", result.status)
                })
                .to_string(),
                is_display: true,
            });
        }

        // No manager attached: report the failure rather than a fabricated
        // one. The previous fallback answered with an agent id, a status of
        // "completed", and an output-file path that was never written — the
        // model had no way to tell that no sub-agent had run, and would go on
        // to read a file that does not exist.
        Err(ToolError::InvalidInput {
            message: format!(
                "cannot run sub-agent '{description}': this session has no agent manager \
                 attached, so no sub-agent was started and no output was produced"
            ),
        })
    }
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    /// Without a manager the tool must not report success: it cannot start a
    /// sub-agent, and claiming otherwise sends the model looking for output
    /// that was never written.
    #[tokio::test]
    async fn agent_without_manager_reports_failure() {
        let result = AgentTool::new()
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({
                        "description": "test agent",
                        "prompt": "do something"
                    }),
                },
                test_ctx(),
            )
            .await;
        match result {
            Err(ToolError::InvalidInput { message }) => {
                assert!(message.contains("no agent manager"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_without_manager_does_not_promise_an_output_file() {
        let result = AgentTool::new()
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({
                        "description": "bg agent",
                        "prompt": "do something",
                        "run_in_background": true
                    }),
                },
                test_ctx(),
            )
            .await;
        assert!(
            result.is_err(),
            "a background agent that was never started must not report an output file"
        );
    }

    /// Delegation carries no rule exemptions. It is not the reference's
    /// "runner" (a CI self-hosted runner), and a session that installs the
    /// exemptions into its engine would otherwise ask about every delegation,
    /// because the fallback for a refused rule is the ordinary default.
    #[test]
    fn agent_declares_no_rule_exemptions() {
        let tool = AgentTool::new();
        assert_eq!(
            tool.capabilities().rule_exemptions(),
            coderex_core::permissions::ToolRuleExemptions::NONE
        );
        assert_eq!(
            tool.rule_exemptions_for(&serde_json::json!({ "prompt": "do something" })),
            coderex_core::permissions::ToolRuleExemptions::NONE
        );
    }

    #[tokio::test]
    async fn agent_missing_description() {
        let result = AgentTool::new()
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({ "prompt": "test" }),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
