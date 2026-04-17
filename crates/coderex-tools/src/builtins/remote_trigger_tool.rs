//! RemoteTrigger tool — trigger remote Bridge work assignment.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Trigger remote Bridge work assignment.
#[derive(Debug)]
pub struct RemoteTriggerTool {
    /// Base URL of the Bridge control plane (e.g. "https://bridge.example.com").
    pub bridge_url: String,
}

#[async_trait]
impl Tool for RemoteTriggerTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "RemoteTrigger".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["assignment_id"],
                "properties": {
                    "assignment_id": { "type": "string", "description": "Work assignment ID to trigger" },
                    "bridge_url": { "type": "string", "description": "Override Bridge control plane URL" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "triggered": { "type": "boolean" },
                    "status": { "type": "string" }
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
        let assignment_id = extract_string(&input.arguments, "assignment_id")?;
        let bridge_url = extract_string(&input.arguments, "bridge_url")
            .unwrap_or_else(|_| self.bridge_url.clone());

        let url = format!("{bridge_url}/api/assignments/{assignment_id}/trigger");
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "assignment_id": assignment_id }).to_string())
            .send()
            .await
            .map_err(|e| ToolError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("HTTP request failed: {e}"),
            )))?;

        let status = response.status();
        let body = response.text().await.map_err(|e| ToolError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to read response: {e}"),
        )))?;

        if status.is_success() {
            Ok(ToolOutput {
                output: serde_json::json!({
                    "triggered": true,
                    "status": format!("assignment {assignment_id} triggered"),
                    "response": body
                }).to_string(),
                is_display: true,
            })
        } else {
            Err(ToolError::InvalidInput {
                message: format!("Bridge returned {status}: {body}"),
            })
        }
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
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"))
    }

    #[tokio::test]
    async fn remote_trigger_fails_without_server() {
        let tool = RemoteTriggerTool {
            bridge_url: "http://localhost:0".into(),
        };
        let result = tool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "RemoteTrigger".into(),
                    arguments: serde_json::json!({ "assignment_id": "abc-123" }),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn remote_trigger_missing_id() {
        let tool = RemoteTriggerTool {
            bridge_url: "http://localhost:0".into(),
        };
        let result = tool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "RemoteTrigger".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn remote_trigger_descriptor() {
        let tool = RemoteTriggerTool {
            bridge_url: "http://example.com".into(),
        };
        let desc = tool.descriptor();
        assert_eq!(desc.name, "RemoteTrigger");
        assert!(matches!(desc.tier, ToolTier::Baseline));
        assert!(!desc.is_read_only);
    }
}
