//! Skill tool — load and execute skill scripts.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Load and execute skill scripts from .coderex/skills/.
#[derive(Debug)]
pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Skill".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["skill"],
                "properties": {
                    "skill": { "type": "string", "description": "Skill name to load" },
                    "args": { "type": "string", "description": "Arguments to pass to the skill" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" }
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
        let skill = extract_string(&input.arguments, "skill")?;
        let args = extract_string_opt(&input.arguments, "args");

        // Search for skill file in multiple locations
        let skill_file = format!("{skill}.md");
        let search_dirs = [
            ctx.cwd.join(".coderex/skills"),
            coderex_config::settings::config_home().join("skills"),
        ];

        for dir in &search_dirs {
            let path = dir.join(&skill_file);
            if path.exists() {
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        let mut output = format!("Skill '{skill}' loaded from {path:?}\n\n");
                        output.push_str(&content);
                        if let Some(a) = args {
                            output.push_str(&format!("\n\nArguments: {a}"));
                        }
                        return Ok(ToolOutput {
                            output,
                            is_display: true,
                        });
                    }
                    Err(e) => {
                        return Err(ToolError::InvalidInput {
                            message: format!("failed to read skill file: {e}"),
                        });
                    }
                }
            }
        }

        // Skill not found — list available skills
        let mut available = Vec::new();
        for dir in &search_dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".md") {
                        available.push(name.trim_end_matches(".md").to_string());
                    }
                }
            }
        }

        let mut msg = format!("Skill '{skill}' not found.");
        if !available.is_empty() {
            msg.push_str(&format!("\nAvailable skills: {}", available.join(", ")));
        } else {
            msg.push_str("\nNo skills found in .coderex/skills/ or ~/.coderex/skills/");
        }
        Err(ToolError::InvalidInput { message: msg })
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
    async fn skill_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".coderex/skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("pdf.md"), "# PDF Skill\nExtract text from PDF files.").unwrap();

        let ctx = ToolContext {
            elicit_tx: None,
            execution_context: ExecutionContext::Normal,
            cwd: tmp.path(),
            extra: Arc::new(HashMap::new()),
        };

        let output = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "pdf" }),
                },
                ctx,
            )
            .await
            .unwrap();
        assert!(output.output.contains("pdf"));
        assert!(output.output.contains("Extract text"));
    }

    #[tokio::test]
    async fn skill_missing_name() {
        let result = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
