//! Edit tool — exact string replacement in files.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Edit file contents with exact string replacement.
#[derive(Debug)]
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Edit".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["file_path", "old_string", "new_string"],
                "properties": {
                    "file_path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "replacements": { "type": "number" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(&self, input: ToolInput, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let file_path = extract_string(&input.arguments, "file_path")?;
        let old_string = extract_string(&input.arguments, "old_string")?;
        let new_string = extract_string(&input.arguments, "new_string")?;
        let replace_all = input.arguments.get("replace_all").and_then(serde_json::Value::as_bool).unwrap_or(false);

        let path = resolve_path(&file_path, ctx.cwd);
        let path_buf = std::path::PathBuf::from(&path);

        // Enforce sandbox path restrictions if a config is present
        if let Some(config) = ctx
            .extra
            .get("sandbox_config")
            .and_then(|v| v.downcast_ref::<coderex_core::sandbox::SandboxConfig>())
        {
            let allowed_read = &config.allowed_read_paths;
            let allowed_write = &config.allowed_write_paths;
            if !crate::sandbox_policy::is_path_allowed(&path_buf, allowed_read) {
                return Err(ToolError::PermissionDenied {
                    name: "Edit".into(),
                });
            }
            if !crate::sandbox_policy::is_path_allowed(&path_buf, allowed_write) {
                return Err(ToolError::PermissionDenied {
                    name: "Edit".into(),
                });
            }
        }

        let content = std::fs::read_to_string(&path_buf).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to read file: {e}"),
        })?;

        let count = content.matches(&old_string).count();
        if count == 0 {
            return Err(ToolError::InvalidInput {
                message: format!("old_string not found in {path}"),
            });
        }
        if count > 1 && !replace_all {
            return Err(ToolError::InvalidInput {
                message: format!("old_string appears {count} times, use replace_all to replace all occurrences"),
            });
        }

        let new_content = if replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };

        std::fs::write(&path_buf, &new_content).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to write file: {e}"),
        })?;

        Ok(ToolOutput {
            output: format!("Replaced {count} occurrence(s) of old_string in {path}"),
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

fn resolve_path(file_path: &str, cwd: &std::path::Path) -> String {
    let path = std::path::PathBuf::from(file_path);
    if path.is_absolute() {
        file_path.to_string()
    } else {
        cwd.join(&path).to_string_lossy().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::path::Path;

    fn test_ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    fn edit_input(file_path: &str, old: &str, new: &str, replace_all: bool) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Edit".into(),
            arguments: serde_json::json!({
                "file_path": file_path,
                "old_string": old,
                "new_string": new,
                "replace_all": replace_all
            }),
        }
    }

    #[tokio::test]
    async fn single_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello world foo").unwrap();

        let output = EditTool
            .execute(edit_input("test.txt", "hello", "hi", false), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert!(output.output.contains("Replaced 1 occurrence"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("test.txt")).unwrap(),
            "hi world foo"
        );
    }

    #[tokio::test]
    async fn replace_all() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "foo bar foo").unwrap();

        let _ = EditTool
            .execute(edit_input("test.txt", "foo", "baz", true), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("test.txt")).unwrap(),
            "baz bar baz"
        );
    }

    #[tokio::test]
    async fn non_unique_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "dup dup dup").unwrap();

        let result = EditTool
            .execute(edit_input("test.txt", "dup", "new", false), test_ctx(tmp.path()))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3 times"));
        assert!(msg.contains("replace_all"));
    }

    #[tokio::test]
    async fn file_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let result = EditTool
            .execute(edit_input("nope.txt", "old", "new", false), test_ctx(tmp.path()))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn old_string_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello").unwrap();

        let result = EditTool
            .execute(edit_input("test.txt", "world", "new", false), test_ctx(tmp.path()))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn no_op_when_strings_equal() {
        let tmp = tempfile::tempdir().unwrap();
        let original = "same content";
        std::fs::write(tmp.path().join("test.txt"), original).unwrap();

        let output = EditTool
            .execute(edit_input("test.txt", "same", "same", false), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert!(output.output.contains("Replaced 1"));
        // Content unchanged (but file was written)
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("test.txt")).unwrap(),
            "same content"
        );
    }
}
