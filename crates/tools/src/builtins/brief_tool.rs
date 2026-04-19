//! Brief tool — generate a session/task summary.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use coderex_core::types::MessageContent;

/// Generate a session or task summary from recent messages.
#[derive(Debug)]
pub struct BriefTool;

#[async_trait]
impl Tool for BriefTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Brief".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session ID to summarize" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let session_id = extract_string_opt(&input.arguments, "session_id")
            .unwrap_or_else(|| "current".into());

        let transcript_dir = ctx.cwd.join(".coderex/transcripts");
        let store = coderex_memory::transcript::TranscriptStore::new(transcript_dir);

        let messages = store
            .read_all(&session_id)
            .map_err(|e| ToolError::InvalidInput {
                message: format!("failed to read transcript: {e}"),
            })?;

        let summary = generate_summary(&session_id, &messages);

        Ok(ToolOutput {
            output: summary,
            is_display: true,
        })
    }
}

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn generate_summary(session_id: &str, messages: &[coderex_core::types::Message]) -> String {
    if messages.is_empty() {
        return format!("No messages found for session '{session_id}'.");
    }

    let total = messages.len();
    let user_count = messages
        .iter()
        .filter(|m| matches!(m.role, coderex_core::types::Role::User))
        .count();
    let assistant_count = messages
        .iter()
        .filter(|m| matches!(m.role, coderex_core::types::Role::Assistant))
        .count();
    let tool_count = messages
        .iter()
        .filter(|m| matches!(m.role, coderex_core::types::Role::Tool))
        .count();
    let compact_count = messages
        .iter()
        .filter(|m| is_compact_boundary(m))
        .count();

    let preview = messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, coderex_core::types::Role::User))
        .and_then(|m| match &m.content {
            MessageContent::Text(t) => {
                let t = t.trim();
                let preview = if t.len() > 80 { &t[..77] } else { t };
                Some(preview.to_string())
            }
            _ => None,
        });

    let mut output = format!("Session Brief: {session_id}\n");
    output.push_str(&format!("Total messages: {total}\n"));
    output.push_str(&format!(
        "  User: {user_count} | Assistant: {assistant_count} | Tool: {tool_count}\n"
    ));
    if compact_count > 0 {
        output.push_str(&format!("  Compactions: {compact_count}\n"));
    }
    if let Some(p) = preview {
        output.push_str(&format!("Last user message: {p}..."));
    }

    output
}

fn is_compact_boundary(msg: &coderex_core::types::Message) -> bool {
    matches!(&msg.content, MessageContent::Text(t) if t.contains("<coderex_compact_boundary>"))
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
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn brief_returns_summary() {
        let output = BriefTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Brief".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("No messages found"));
    }

    #[tokio::test]
    async fn brief_with_session_id() {
        let output = BriefTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Brief".into(),
                    arguments: serde_json::json!({ "session_id": "abc-123" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("abc-123"));
    }

    #[tokio::test]
    async fn brief_reads_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript_dir = tmp.path().join(".coderex/transcripts");
        std::fs::create_dir_all(&transcript_dir).unwrap();

        let store = coderex_memory::transcript::TranscriptStore::new(transcript_dir);
        store
            .append("sess_1", &coderex_core::types::Message::user("hello world"))
            .unwrap();
        store
            .append("sess_1", &coderex_core::types::Message::assistant("hi there"))
            .unwrap();

        let ctx = ToolContext {
            execution_context: ExecutionContext::Normal,
            cwd: tmp.path(),
            extra: Arc::new(HashMap::new()),
        };

        let output = BriefTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Brief".into(),
                    arguments: serde_json::json!({ "session_id": "sess_1" }),
                },
                ctx,
            )
            .await
            .unwrap();

        assert!(output.output.contains("Total messages: 2"));
        assert!(output.output.contains("User: 1"));
        assert!(output.output.contains("Assistant: 1"));
    }
}
