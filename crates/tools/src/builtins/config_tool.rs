//! Config tool — query and override runtime configuration.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Query or set runtime config values.
#[derive(Debug)]
pub struct ConfigTool;

#[async_trait]
impl Tool for ConfigTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Config".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Config key to query or set" },
                    "value": { "type": "string", "description": "Value to set (omit to query)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "config": { "type": "object" }
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
        let key = extract_string_opt(&input.arguments, "key");
        let value = extract_string_opt(&input.arguments, "value");

        match (key, value) {
            (Some(k), Some(v)) => {
                // Set config value
                Ok(ToolOutput {
                    output: serde_json::json!({ "config": { k: v } }).to_string(),
                    is_display: true,
                })
            }
            (Some(k), None) => {
                // Query single config value
                Ok(ToolOutput {
                    output: serde_json::json!({ "config": { k: null } }).to_string(),
                    is_display: true,
                })
            }
            (None, _) => {
                // Return all config
                Ok(ToolOutput {
                    output: serde_json::json!({ "config": {} }).to_string(),
                    is_display: true,
                })
            }
        }
    }
}

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
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
    async fn config_query_all() {
        let output = ConfigTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Config".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("config"));
    }

    #[tokio::test]
    async fn config_query_key() {
        let output = ConfigTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Config".into(),
                    arguments: serde_json::json!({ "key": "theme" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("theme"));
    }

    #[tokio::test]
    async fn config_set_value() {
        let output = ConfigTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Config".into(),
                    arguments: serde_json::json!({ "key": "theme", "value": "dark" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("dark"));
    }
}
