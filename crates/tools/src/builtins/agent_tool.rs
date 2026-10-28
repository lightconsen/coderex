//! Agent tool — spawn a sub-agent via AgentManager.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{
    Backgrounding, Tool, ToolCapabilities, ToolContext, ToolDescriptor, ToolInput, ToolTier,
};
use async_trait::async_trait;
use coderex_agents::ParentContextSnapshot;
use std::sync::Arc;

/// Provider for the parent conversation snapshot used to build a cache-safe
/// fork. Called once per spawn, so it always sees the conversation as it is at
/// that moment.
pub type ParentContextProvider = dyn Fn() -> Option<ParentContextSnapshot> + Send + Sync + 'static;

/// Spawn a sub-agent running coderex with the given prompt.
pub struct AgentTool {
    manager: Option<Arc<tokio::sync::Mutex<coderex_agents::manager::AgentManager>>>,
    session_id: String,
    cwd: String,
    parent_context: Option<Arc<ParentContextProvider>>,
}

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTool")
            .field("has_manager", &self.manager.is_some())
            .field("session_id", &self.session_id)
            .field("cwd", &self.cwd)
            .field("has_parent_context", &self.parent_context.is_some())
            .finish()
    }
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
            parent_context: None,
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

    /// Attach a provider for the parent conversation snapshot.
    ///
    /// When present, every spawn records the parent's messages and the
    /// request components that make a prefix-cache hit safe, so the child's
    /// [`coderex_agents::ForkedContext`] is cache-aware instead of starting a
    /// fresh prefix.
    #[must_use]
    pub fn with_parent_context_provider<P>(mut self, provider: P) -> Self
    where
        P: Fn() -> Option<ParentContextSnapshot> + Send + Sync + 'static,
    {
        self.parent_context = Some(Arc::new(provider));
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
                    "agent": {
                        "type": "string",
                        "description": "Name of a custom agent defined in ~/.coderex/agents/*.json. Takes precedence over subagent_type. An unknown name is an error that lists the configured names."
                    },
                    "subagent_type": {
                        "type": "string",
                        "description": "Built-in agent type (general_purpose, explore, plan, verification, claude_code_guide, statusline_setup), or a configured custom agent name."
                    },
                    "model": { "type": "string" },
                    "run_in_background": { "type": "boolean" },
                    "isolation": { "type": "string" },
                    "writable": {
                        "type": "boolean",
                        "description": "Whether the sub-agent may write files. false restricts it to read-only tools (Read, Grep, Glob). Defaults to true."
                    },
                    "read_only": {
                        "type": "boolean",
                        "description": "Inverse convenience alias for writable:false."
                    },
                    "retry_policy": {
                        "type": "object",
                        "description": "Retry behavior on failure.",
                        "properties": {
                            "max_retries": { "type": "integer", "minimum": 0 },
                            "on_failure": { "type": "string", "enum": ["retry", "escalate_to_parent"] },
                            "escalation_prompt": { "type": "string" }
                        }
                    },
                    "max_retries": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Shorthand for retry_policy.max_retries when retry_policy is not supplied."
                    }
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
            .and_then(serde_json::Value::as_bool)
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
        let named_agent = input
            .arguments
            .get("agent")
            .and_then(|v| v.as_str())
            .map(String::from);
        let writable = input
            .arguments
            .get("writable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
            && !input
                .arguments
                .get("read_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        let retry_policy = parse_retry_policy(&input.arguments)?;

        if let Some(ref manager) = self.manager {
            use coderex_agents::manager::SpawnOptions;
            use coderex_agents::types::AgentType;

            let builtin_type = match subagent_type {
                "explore" | "research" => AgentType::Explore {
                    thoroughness: coderex_agents::types::Thoroughness::Medium,
                },
                "plan" => AgentType::Plan,
                "verification" | "verify" => AgentType::Verification,
                "claude_code_guide" => AgentType::ClaudeCodeGuide,
                "statusline_setup" => AgentType::StatuslineSetup,
                _ => AgentType::GeneralPurpose,
            };

            let mut options = SpawnOptions {
                run_in_background: background,
                model_override,
                isolation,
                writable,
                retry_policy,
                ..Default::default()
            };
            let mut message_count = 0;
            if let Some(snapshot) = self.parent_context.as_ref().and_then(|provider| provider())
                && snapshot.has_prefix()
            {
                options.parent_cache_safe_params = Some(snapshot.cache_safe_params());
                message_count = snapshot.messages.len();
                options.parent_prefix = snapshot.messages;
            }

            let mut mgr = manager.lock().await;
            // A name explicitly supplied via `agent` is always resolved as a
            // custom agent: a typo must fail with the available names rather
            // than silently falling back to GeneralPurpose. An unrecognised
            // `subagent_type` is only treated as custom when it actually
            // matches a configured definition.
            let named = named_agent.or_else(|| {
                let is_builtin = matches!(
                    subagent_type,
                    "general_purpose"
                        | "explore"
                        | "research"
                        | "plan"
                        | "verification"
                        | "verify"
                        | "claude_code_guide"
                        | "statusline_setup"
                );
                (!is_builtin && mgr.has_custom_agent(subagent_type))
                    .then(|| subagent_type.to_string())
            });

            let instance = if let Some(name) = named {
                mgr.spawn_named_agent(
                    &name,
                    &prompt,
                    options,
                    &self.session_id,
                    &self.cwd,
                    message_count,
                )
                .await
            } else {
                mgr.spawn_agent(
                    builtin_type,
                    &prompt,
                    options,
                    &self.session_id,
                    &self.cwd,
                    message_count,
                )
                .await
            }
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
                    images: Vec::new(),
                });
            }

            // Synchronous: wait for completion and return the agent's output
            let result =
                mgr.wait_for_completion(&agent_id)
                    .await
                    .map_err(|e| ToolError::InvalidInput {
                        message: format!("agent execution failed: {e}"),
                    })?;
            drop(mgr);

            return Ok(ToolOutput {
                output: serde_json::json!({
                    "agent_id": agent_id,
                    "output_file": output_file,
                    "output": result.output,
                    "status": format!("{:?}", result.status)
                })
                .to_string(),
                is_display: true,
                images: Vec::new(),
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

/// Parse the retry configuration from the tool input.
///
/// `retry_policy` is the full object; `max_retries` is a convenience
/// shorthand used only when no full policy was supplied.
fn parse_retry_policy(
    args: &serde_json::Value,
) -> Result<Option<coderex_agents::RetryPolicy>, ToolError> {
    if let Some(value) = args.get("retry_policy") {
        let policy =
            serde_json::from_value(value.clone()).map_err(|error| ToolError::InvalidInput {
                message: format!("invalid retry_policy: {error}"),
            })?;
        return Ok(Some(policy));
    }

    Ok(args
        .get("max_retries")
        .and_then(serde_json::Value::as_u64)
        .map(|max_retries| coderex_agents::RetryPolicy {
            max_retries: u32::try_from(max_retries).unwrap_or(u32::MAX),
            ..Default::default()
        }))
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
            path_verifier: None,
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

    fn manager_with_reviewer() -> Arc<tokio::sync::Mutex<coderex_agents::AgentManager>> {
        let mut manager = coderex_agents::AgentManager::standalone();
        manager.enable_test_mode();
        manager.register_custom_agent(coderex_agents::CustomAgentDef {
            name: "reviewer".into(),
            system_prompt: "You review code carefully.".into(),
            allowed_tools: vec!["Read".into(), "Grep".into()],
            model: None,
            max_tokens: None,
            output_schema: None,
        });
        Arc::new(tokio::sync::Mutex::new(manager))
    }

    fn tool_with_manager(
        manager: Arc<tokio::sync::Mutex<coderex_agents::AgentManager>>,
    ) -> AgentTool {
        AgentTool::new()
            .with_manager(manager)
            .with_session_id("test-session")
            .with_cwd("/tmp")
    }

    #[tokio::test]
    async fn agent_tool_spawns_named_custom_agent() {
        let manager = manager_with_reviewer();
        let result = tool_with_manager(Arc::clone(&manager))
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({
                        "description": "review",
                        "prompt": "review auth.rs",
                        "agent": "reviewer"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(
            result.output.contains("\"status\":\"Completed\""),
            "unexpected agent tool output: {}",
            result.output
        );

        let manager = manager.lock().await;
        let agents = manager.list_agents();
        let agent = agents.first().expect("agent not spawned");
        assert!(
            agent.prompt.contains("You review code carefully."),
            "custom system prompt not applied: {}",
            agent.prompt
        );
        assert_eq!(
            agent.options.allowed_tools,
            Some(vec!["Read".into(), "Grep".into()])
        );
    }

    #[tokio::test]
    async fn agent_tool_unknown_custom_agent_lists_names() {
        let manager = manager_with_reviewer();
        let result = tool_with_manager(manager)
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({
                        "description": "review",
                        "prompt": "do work",
                        "agent": "typo"
                    }),
                },
                test_ctx(),
            )
            .await;

        match result {
            Err(ToolError::InvalidInput { message }) => {
                assert!(message.contains("unknown custom agent"), "{message}");
                assert!(message.contains("reviewer"), "{message}");
            }
            other => panic!("expected an unknown-agent refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_tool_passes_writable_and_retry_policy_through() {
        let manager = manager_with_reviewer();
        tool_with_manager(Arc::clone(&manager))
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Agent".into(),
                    arguments: serde_json::json!({
                        "description": "read only",
                        "prompt": "read the file",
                        "read_only": true,
                        "retry_policy": {
                            "max_retries": 3,
                            "on_failure": "retry",
                            "escalation_prompt": "try again"
                        }
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();

        let manager = manager.lock().await;
        let agents = manager.list_agents();
        let agent = agents.first().expect("agent not spawned");
        assert!(!agent.options.writable);
        let policy = agent
            .options
            .retry_policy
            .as_ref()
            .expect("retry policy lost");
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.on_failure, coderex_agents::FailureAction::Retry);
        assert_eq!(policy.escalation_prompt.as_deref(), Some("try again"));
    }

    #[test]
    fn partial_retry_policy_falls_back_to_defaults() {
        let policy = parse_retry_policy(&serde_json::json!({
            "retry_policy": { "max_retries": 1 }
        }))
        .unwrap()
        .expect("policy missing");
        assert_eq!(policy.max_retries, 1);
        assert_eq!(
            policy.on_failure,
            coderex_agents::FailureAction::EscalateToParent
        );
        assert!(policy.escalation_prompt.is_none());
    }

    #[tokio::test]
    async fn agent_tool_records_cache_safe_parent_prefix() {
        let manager = manager_with_reviewer();
        let snapshot = coderex_agents::ParentContextSnapshot::new(
            vec![
                serde_json::json!({"role": "user", "content": "first"}),
                serde_json::json!({"role": "assistant", "content": "answer"}),
            ],
            "claude-test",
            "parent system prompt",
            vec!["Read".into(), "Grep".into()],
        );
        let tool = tool_with_manager(Arc::clone(&manager))
            .with_parent_context_provider(move || Some(snapshot.clone()));

        tool.execute(
            ToolInput {
                call_id: "1".into(),
                name: "Agent".into(),
                arguments: serde_json::json!({
                    "description": "delegate",
                    "prompt": "continue the work"
                }),
            },
            test_ctx(),
        )
        .await
        .unwrap();

        let manager = manager.lock().await;
        let agents = manager.list_agents();
        let agent = agents.first().expect("agent not spawned");
        let context = agent.context.as_ref().expect("missing forked context");
        assert!(context.reuses_parent_prefix());
        assert_eq!(context.fork_point, 2);
        assert_eq!(context.fork_messages("task").len(), 3);
        assert!(context.is_cache_safe(
            "claude-test",
            "parent system prompt",
            &["Read".into(), "Grep".into()]
        ));
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
