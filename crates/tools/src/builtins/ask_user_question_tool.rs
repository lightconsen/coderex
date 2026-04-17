//! AskUserQuestion tool — ask the user questions during execution.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Ask the user questions during execution.
#[derive(Debug)]
pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "AskUserQuestion".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["questions"],
                "properties": {
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["question", "header", "options"],
                            "properties": {
                                "question": { "type": "string" },
                                "header": { "type": "string" },
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": ["label", "description"],
                                        "properties": {
                                            "label": { "type": "string" },
                                            "description": { "type": "string" }
                                        }
                                    }
                                },
                                "multiSelect": { "type": "boolean" }
                            }
                        }
                    }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "answers": { "type": "object" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let questions = input.arguments.get("questions").ok_or_else(|| ToolError::InvalidInput {
            message: "missing required field: questions".into(),
        })?;

        // In a real implementation, this would use an elicitation channel
        // to present questions to the user and wait for their response.
        // For now, return a placeholder indicating the question was received.
        let count = questions.as_array().map(|a| a.len()).unwrap_or(0);

        Ok(ToolOutput {
            output: serde_json::json!({
                "answers": {},
                "_note": format!("{count} question(s) received — user interaction not available in this context")
            }).to_string(),
            is_display: true,
        })
    }
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
    async fn ask_user_question_returns_placeholder() {
        let output = AskUserQuestionTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "AskUserQuestion".into(),
                    arguments: serde_json::json!({
                        "questions": [{
                            "question": "Which library?",
                            "header": "Library",
                            "options": [
                                { "label": "A", "description": "Option A" },
                                { "label": "B", "description": "Option B" }
                            ]
                        }]
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("question"));
    }

    #[tokio::test]
    async fn ask_user_question_missing_questions() {
        let result = AskUserQuestionTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "AskUserQuestion".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
