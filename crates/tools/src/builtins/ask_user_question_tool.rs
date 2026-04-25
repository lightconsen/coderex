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

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let questions =
            input
                .arguments
                .get("questions")
                .ok_or_else(|| ToolError::InvalidInput {
                    message: "missing required field: questions".into(),
                })?;

        let question_values: Vec<serde_json::Value> = questions
            .as_array()
            .ok_or_else(|| ToolError::InvalidInput {
                message: "questions must be an array".into(),
            })?
            .clone();

        let tx = ctx.elicit_tx.ok_or(ToolError::ElicitationUnavailable)?;
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        tx.send(crate::tool::ElicitRequest {
            questions: question_values,
            response_tx,
        })
        .await
        .map_err(|_| ToolError::ElicitationUnavailable)?;

        let answers = response_rx
            .await
            .map_err(|_| ToolError::ElicitationUnavailable)?;

        Ok(ToolOutput {
            output: serde_json::json!({ "answers": answers }).to_string(),
            is_display: true,
        })
    }
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

    #[tokio::test]
    async fn ask_user_question_without_channel_errors() {
        let result = AskUserQuestionTool
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
            .await;
        assert!(matches!(result, Err(ToolError::ElicitationUnavailable)));
    }

    #[tokio::test]
    async fn ask_user_question_with_channel_receives_answers() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let ctx = ToolContext {
            elicit_tx: Some(tx),
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        };

        let handle = tokio::spawn(async move {
            AskUserQuestionTool
                .execute(
                    ToolInput {
                        call_id: "1".into(),
                        name: "AskUserQuestion".into(),
                        arguments: serde_json::json!({
                            "questions": [{
                                "question": "Which library?",
                                "header": "Library",
                                "options": [
                                    { "label": "A", "description": "Option A" }
                                ]
                            }]
                        }),
                    },
                    ctx,
                )
                .await
        });

        let request = rx.recv().await.expect("should receive elicit request");
        assert_eq!(request.questions.len(), 1);
        let _ = request
            .response_tx
            .send(serde_json::json!({ "answer": "A" }));

        let output = handle.await.unwrap().unwrap();
        assert!(output.output.contains("A"));
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
