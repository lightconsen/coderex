//! ToolSearch tool — search available tools by name/description.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Search for available tools by name or description.
#[derive(Debug)]
pub struct ToolSearchTool;

#[async_trait]
impl Tool for ToolSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ToolSearch".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search query for tool name or description" },
                    "limit": { "type": "number", "description": "Maximum number of results (default 10)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tools": { "type": "array", "items": { "type": "string" } }
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
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let query = extract_string(&input.arguments, "query")?;
        let limit = extract_number(&input.arguments, "limit").unwrap_or(10) as usize;

        let query_lower = query.to_lowercase();

        // Search through the registry's baseline tools
        let registry = ctx
            .extra
            .get("tool_registry")
            .and_then(|v| v.downcast_ref::<Arc<crate::registry::ToolRegistry>>());

        let mut results = Vec::new();

        if let Some(reg) = registry {
            let tools = reg.list_tools();
            for (name, tool) in tools {
                let desc = tool.descriptor();
                let name_matches = name.to_lowercase().contains(&query_lower);
                let desc_matches = desc.name.to_lowercase().contains(&query_lower);

                if name_matches || desc_matches {
                    let schema_str = serde_json::to_string(&desc.input_schema).unwrap_or_default();
                    results.push(format!(
                        "- **{name}** (tier: {:?})\n  Input schema: {schema_str}",
                        desc.tier
                    ));
                }

                if results.len() >= limit {
                    break;
                }
            }
        }

        let output = if results.is_empty() {
            format!("No tools found matching \"{query}\"")
        } else {
            format!(
                "Found {} tool(s) matching \"{query}\":\n\n{}",
                results.len(),
                results.join("\n\n")
            )
        };

        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

use std::sync::Arc;

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

fn extract_number(args: &serde_json::Value, key: &str) -> Option<i64> {
    args.get(key).and_then(serde_json::Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::{EchoTool, ReadTool};
    use crate::context::ExecutionContext;
    use crate::registry::ToolRegistry;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn ctx_with_registry() -> ToolContext<'static> {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        reg.register(Arc::new(ReadTool));
        let mut extra = HashMap::<String, crate::tool::SharedState>::new();
        extra.insert("tool_registry".to_string(), Box::new(Arc::new(reg)));
        ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(extra),
        }
    }

    fn toolsearch_input(query: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "ToolSearch".into(),
            arguments: serde_json::json!({ "query": query }),
        }
    }

    #[tokio::test]
    async fn toolsearch_finds_matching_tool() {
        let ctx = ctx_with_registry();
        let output = ToolSearchTool
            .execute(toolsearch_input("Read"), ctx)
            .await
            .unwrap();
        assert!(output.output.contains("Read"));
    }

    #[tokio::test]
    async fn toolsearch_no_matches() {
        let ctx = ctx_with_registry();
        let output = ToolSearchTool
            .execute(toolsearch_input("nonexistent_xyz"), ctx)
            .await
            .unwrap();
        assert!(output.output.contains("No tools found"));
    }

    #[tokio::test]
    async fn toolsearch_missing_query() {
        let input = ToolInput {
            call_id: "1".into(),
            name: "ToolSearch".into(),
            arguments: serde_json::json!({}),
        };
        let ctx = ctx_with_registry();
        let result = ToolSearchTool.execute(input, ctx).await;
        assert!(result.is_err());
    }
}
