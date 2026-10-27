//! ListMcpPrompts / GetMcpPrompt baseline tools.
//!
//! Like the resource tools, these drive an injected `McpPromptHandle` that the
//! runtime wires up from the `coderex-mcp` crate. The small trait keeps the
//! `tools` -> `mcp` dependency out of this crate.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use serde_json::{Map, Value};

/// Shared-state key for an injected `McpPromptHandle`.
pub const MCP_PROMPT_HANDLE_KEY: &str = "mcp_prompt_handle";

/// One argument declared by an MCP prompt.
#[derive(Debug, Clone)]
pub struct McpPromptArgumentEntry {
    /// Argument name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Whether the server requires a value.
    pub required: bool,
}

/// An MCP prompt descriptor returned by the handle.
#[derive(Debug, Clone)]
pub struct McpPromptEntry {
    /// Name of the server that advertises the prompt.
    pub server: String,
    /// Prompt name, as sent to `prompts/get`.
    pub name: String,
    /// What the prompt is for.
    pub description: Option<String>,
    /// Arguments the prompt accepts.
    pub arguments: Vec<McpPromptArgumentEntry>,
}

/// Abstracts MCP prompt discovery and resolution for this tool layer.
pub trait McpPromptHandle: Send + Sync {
    /// List every prompt discovered across connected servers.
    fn list(&self) -> Vec<McpPromptEntry>;
    /// Resolve one prompt, returning its messages rendered as text.
    fn get(
        &self,
        server: &str,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<String, String>;
}

fn get_handle(ctx: &ToolContext<'_>) -> Result<std::sync::Arc<dyn McpPromptHandle>, ToolError> {
    ctx.extra
        .get(MCP_PROMPT_HANDLE_KEY)
        .and_then(|value| value.downcast_ref::<std::sync::Arc<dyn McpPromptHandle>>())
        .cloned()
        .ok_or_else(|| ToolError::InvalidInput {
            message: "MCP prompt handle not available in this context".into(),
        })
}

/// List MCP prompts discovered across connected servers.
#[derive(Debug)]
pub struct ListMcpPromptsTool;

#[async_trait]
impl Tool for ListMcpPromptsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ListMcpPrompts".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompts": {
                        "type": "array",
                        "items": { "type": "object" }
                    }
                }
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
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let handle = get_handle(&ctx)?;
        let prompts: Vec<_> = handle
            .list()
            .into_iter()
            .map(|prompt| {
                let arguments: Vec<_> = prompt
                    .arguments
                    .into_iter()
                    .map(|argument| {
                        serde_json::json!({
                            "name": argument.name,
                            "description": argument.description,
                            "required": argument.required,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "server": prompt.server,
                    "name": prompt.name,
                    "description": prompt.description,
                    "arguments": arguments,
                })
            })
            .collect();
        Ok(ToolOutput {
            output: serde_json::to_string_pretty(&prompts).unwrap_or_else(|_| "[]".into()),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Resolve a named MCP prompt and return its messages as text.
#[derive(Debug)]
pub struct GetMcpPromptTool;

#[async_trait]
impl Tool for GetMcpPromptTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "GetMcpPrompt".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["server", "name"],
                "properties": {
                    "server": { "type": "string" },
                    "name": { "type": "string" },
                    "arguments": {
                        "type": "object",
                        "additionalProperties": { "type": "string" }
                    }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "content": { "type": "string" } }
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
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let server = input
            .arguments
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: server".into(),
            })?;
        let name = input
            .arguments
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: name".into(),
            })?;
        let arguments = match input.arguments.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => {
                return Err(ToolError::InvalidInput {
                    message: "'arguments' must be an object".into(),
                });
            }
        };

        let handle = get_handle(&ctx)?;
        let content = handle
            .get(server, name, arguments)
            .map_err(|message| ToolError::InvalidInput { message })?;
        Ok(ToolOutput {
            output: content,
            is_display: true,
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    struct StubHandle;

    impl McpPromptHandle for StubHandle {
        fn list(&self) -> Vec<McpPromptEntry> {
            vec![McpPromptEntry {
                server: "mock".into(),
                name: "review".into(),
                description: Some("Review a file".into()),
                arguments: vec![McpPromptArgumentEntry {
                    name: "path".into(),
                    description: None,
                    required: true,
                }],
            }]
        }

        fn get(
            &self,
            server: &str,
            name: &str,
            arguments: Map<String, Value>,
        ) -> Result<String, String> {
            if server == "mock" && name == "review" {
                Ok(format!(
                    "reviewing {}",
                    arguments.get("path").and_then(Value::as_str).unwrap_or("?")
                ))
            } else {
                Err(format!("no prompt '{name}' on server '{server}'"))
            }
        }
    }

    fn ctx_with_handle() -> ToolContext<'static> {
        let handle: Arc<dyn McpPromptHandle> = Arc::new(StubHandle);
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(MCP_PROMPT_HANDLE_KEY.into(), Box::new(handle));
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    #[tokio::test]
    async fn list_prompts_returns_server_and_arguments() {
        let output = ListMcpPromptsTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "ListMcpPrompts".into(),
                    arguments: serde_json::json!({}),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap();
        assert!(
            output.output.contains("\"server\": \"mock\""),
            "{}",
            output.output
        );
        assert!(output.output.contains("\"name\": \"review\""));
        assert!(output.output.contains("\"required\": true"));
    }

    #[tokio::test]
    async fn get_prompt_forwards_arguments() {
        let output = GetMcpPromptTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "GetMcpPrompt".into(),
                    arguments: serde_json::json!({
                        "server": "mock",
                        "name": "review",
                        "arguments": {"path": "src/lib.rs"}
                    }),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "reviewing src/lib.rs");
    }

    #[tokio::test]
    async fn get_prompt_reports_an_unknown_prompt() {
        let error = GetMcpPromptTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "GetMcpPrompt".into(),
                    arguments: serde_json::json!({"server": "mock", "name": "missing"}),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no prompt 'missing'"));
    }

    #[tokio::test]
    async fn get_prompt_requires_server_and_name() {
        let error = GetMcpPromptTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "GetMcpPrompt".into(),
                    arguments: serde_json::json!({"name": "review"}),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput { .. }));
        assert!(error.to_string().contains("server"));

        let error = GetMcpPromptTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "GetMcpPrompt".into(),
                    arguments: serde_json::json!({"server": "mock"}),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("name"));
    }

    #[tokio::test]
    async fn get_prompt_refuses_non_object_arguments() {
        let error = GetMcpPromptTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "GetMcpPrompt".into(),
                    arguments: serde_json::json!({
                        "server": "mock",
                        "name": "review",
                        "arguments": "path=src/lib.rs"
                    }),
                },
                ctx_with_handle(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("must be an object"));
    }
}
