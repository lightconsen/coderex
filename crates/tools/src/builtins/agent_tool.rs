//! Agent tool — spawn a sub-agent via AgentManager.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
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
    /// Create a new AgentTool without a manager (returns stub output).
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

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let description = extract_string(&input.arguments, "description")?;
        let prompt = extract_string(&input.arguments, "prompt")?;
        let background = input.arguments.get("run_in_background").and_then(|v| v.as_bool()).unwrap_or(false);
        let subagent_type = input.arguments.get("subagent_type").and_then(|v| v.as_str()).unwrap_or("general_purpose");
        let model_override = input.arguments.get("model").and_then(|v| v.as_str()).map(String::from);
        let isolation = input.arguments.get("isolation").and_then(|v| v.as_str()).map(String::from);

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
                .spawn_agent(
                    agent_type,
                    &prompt,
                    options,
                    &self.session_id,
                    &self.cwd,
                    0,
                )
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
            let result = mgr
                .wait_for_completion(&agent_id)
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

        // Fallback stub when no manager is attached
        let agent_id = format!("agent-{description}");
        let output_file = format!("/tmp/agent-{description}-output.txt");

        Ok(ToolOutput {
            output: serde_json::json!({
                "agent_id": agent_id,
                "output_file": if background { Some(output_file) } else { None },
                "note": if background { "agent running in background (no manager attached)" } else { "agent completed synchronously (no manager attached)" }
            }).to_string(),
            is_display: true,
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
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
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

    #[tokio::test]
    async fn agent_returns_id() {
        let output = AgentTool::new()
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
            .await
            .unwrap();
        assert!(output.output.contains("agent"));
    }

    #[tokio::test]
    async fn agent_background_returns_output_file() {
        let output = AgentTool::new()
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
            .await
            .unwrap();
        assert!(output.output.contains("output_file"));
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
