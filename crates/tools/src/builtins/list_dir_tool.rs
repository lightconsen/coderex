//! ListDir tool — list directory entries without executing a shell.

use std::path::{Path, PathBuf};

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Maximum number of directory entries to display.
const MAX_ENTRIES: usize = 500;

/// List the contents of a directory.
#[derive(Debug)]
pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ListDir".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list (defaults to cwd)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "entries": { "type": "array", "items": { "type": "string" } }
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
        let dir = input
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| ctx.cwd.to_path_buf(), |path| resolve_path(path, ctx.cwd));

        let metadata = std::fs::metadata(&dir).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to access directory: {e}"),
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::InvalidInput {
                message: format!("not a directory: {}", dir.display()),
            });
        }

        let mut entries = std::fs::read_dir(&dir)
            .map_err(|e| ToolError::InvalidInput {
                message: format!("failed to list directory: {e}"),
            })?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if matches!(name.as_str(), "." | "..") {
                    return None;
                }
                let suffix = entry
                    .metadata()
                    .ok()
                    .filter(std::fs::Metadata::is_dir)
                    .map_or("", |_| "/");
                Some(format!("{name}{suffix}"))
            })
            .collect::<Vec<_>>();

        entries.sort();
        if entries.len() > MAX_ENTRIES {
            entries.truncate(MAX_ENTRIES);
            entries.push(format!("... (truncated to {MAX_ENTRIES} entries)"));
        }

        Ok(ToolOutput {
            output: entries.join("\n"),
            is_display: true,
            images: Vec::new(),
        })
    }
}

fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn list_dir_lists_files_and_hidden_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join(".coderex")).unwrap();

        let output = ListDirTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "ListDir".into(),
                    arguments: serde_json::json!({}),
                },
                ToolContext::new(ExecutionContext::Normal, tmp.path()),
            )
            .await
            .unwrap();

        assert!(output.output.contains("Cargo.toml"));
        assert!(output.output.contains("src/"));
        assert!(output.output.contains(".coderex/"));
    }
}
