//! Write tool — write file contents, creating parent directories.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Write file contents, creating parent directories if needed.
#[derive(Debug)]
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Write".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["file_path", "content"],
                "properties": {
                    "file_path": { "type": "string" },
                    "content": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "bytes_written": { "type": "number" }
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
        let content = extract_string(&input.arguments, "content")?;

        let path = resolve_path(&file_path, ctx.cwd);
        let path_buf = std::path::PathBuf::from(&path);

        // Enforce sandbox path restrictions if a config is present
        if let Some(config) = ctx
            .extra
            .get("sandbox_config")
            .and_then(|v| v.downcast_ref::<coderex_core::sandbox::SandboxConfig>())
        {
            let allowed_write = &config.allowed_write_paths;
            if !crate::sandbox_policy::is_path_allowed(&path_buf, allowed_write) {
                return Err(ToolError::PermissionDenied {
                    name: "Write".into(),
                });
            }
            if let Some(parent) = path_buf.parent() {
                if !crate::sandbox_policy::is_path_allowed(parent, allowed_write) {
                    return Err(ToolError::PermissionDenied {
                        name: "Write".into(),
                    });
                }
            }
        }

        if let Some(parent) = path_buf.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ToolError::InvalidInput {
                message: format!("failed to create directory: {e}"),
            })?;
        }

        std::fs::write(&path_buf, &content).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to write file: {e}"),
        })?;

        let bytes = content.len();
        Ok(ToolOutput {
            output: format!("Wrote {bytes} bytes to {path}"),
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

    fn test_input(file_path: &str, content: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Write".into(),
            arguments: serde_json::json!({ "file_path": file_path, "content": content }),
        }
    }

    #[tokio::test]
    async fn write_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let output = WriteTool
            .execute(test_input("new.txt", "hello"), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert!(output.output.contains("Wrote"));
        assert!(output.output.contains("bytes"));
        assert!(tmp.path().join("new.txt").exists());
        assert_eq!(std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(), "hello");
    }

    #[tokio::test]
    async fn write_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("existing.txt"), "old").unwrap();

        let _ = WriteTool
            .execute(test_input("existing.txt", "new"), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("existing.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = WriteTool
            .execute(test_input("a/b/c/file.txt", "nested"), test_ctx(tmp.path()))
            .await
            .unwrap();
        let file = tmp.path().join("a/b/c/file.txt");
        assert!(file.exists());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "nested");
    }

    #[tokio::test]
    async fn write_empty_content() {
        let tmp = tempfile::tempdir().unwrap();
        let output = WriteTool
            .execute(test_input("empty.txt", ""), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert!(output.output.contains("Wrote 0 bytes"));
        assert!(tmp.path().join("empty.txt").exists());
        assert_eq!(std::fs::read_to_string(tmp.path().join("empty.txt")).unwrap(), "");
    }
}
