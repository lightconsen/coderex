//! Write tool — write file contents, creating parent directories.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::io::{Seek, SeekFrom, Write as _};

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

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
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

        // Open first, then verify the handle's real path. Truncation happens
        // only after the path the OS resolved has passed the permission check,
        // so a retargeted symlink cannot redirect this write.
        let mut verified = crate::verified_fs::open_write(&path_buf, &ctx)?;
        let actual = verified.path.clone();
        if let Some(config) = ctx
            .extra
            .get("sandbox_config")
            .and_then(|value| value.downcast_ref::<coderex_core::sandbox::SandboxConfig>())
            && !crate::sandbox_policy::is_path_allowed(&actual, &config.allowed_write_paths)
        {
            return Err(ToolError::PermissionDenied {
                name: "Write".into(),
            });
        }

        // Snapshot what is about to be overwritten so `/rewind` can undo this
        // write along with the conversation truncation.
        if let Some(store) = crate::tool::checkpoint_store_from(&ctx) {
            if verified.created {
                store.snapshot_absent(&input.call_id, &actual);
            } else {
                store.snapshot(&input.call_id, &actual);
            }
        }

        verified
            .file
            .set_len(0)
            .and_then(|()| verified.file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| verified.file.write_all(content.as_bytes()))
            .map_err(|e| ToolError::InvalidInput {
                message: format!("failed to write file: {e}"),
            })?;

        let bytes = content.len();
        Ok(ToolOutput {
            output: format!("Wrote {bytes} bytes to {path}"),
            is_display: true,
            images: Vec::new(),
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
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    /// A session whose dispatcher installed a checkpoint store: the tool must
    /// snapshot what it is about to overwrite, so `/rewind` can undo the write.
    #[tokio::test]
    async fn a_write_snapshots_what_it_overwrites_when_a_store_is_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "v1").unwrap();

        let store = std::sync::Arc::new(
            coderex_core::checkpoints::CheckpointStore::new(&tmp.path().join("history")).unwrap(),
        );
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            crate::tool::CHECKPOINT_STORE_KEY.to_string(),
            Box::new(std::sync::Arc::clone(&store)) as crate::tool::SharedState,
        );
        let ctx = ToolContext::new(ExecutionContext::Normal, tmp.path()).with_extra(extra);

        let output = WriteTool
            .execute(test_input("a.txt", "v2"), ctx)
            .await
            .unwrap();
        assert!(output.output.contains("Wrote"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2");

        let undone: std::collections::HashSet<String> = ["1".to_string()].into_iter().collect();
        let report = store.restore(&undone);
        assert_eq!(
            report.restored,
            vec![std::fs::canonicalize(&target).unwrap()]
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1");
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
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "hello"
        );
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
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("empty.txt")).unwrap(),
            ""
        );
    }
}

#[cfg(test)]
mod verified_write_tests {
    use super::*;
    use crate::context::ExecutionContext;
    use std::sync::Arc;

    fn ctx(cwd: &std::path::Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    fn input(file_path: &str, content: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Write".into(),
            arguments: serde_json::json!({ "file_path": file_path, "content": content }),
        }
    }

    #[tokio::test]
    async fn a_denied_handle_is_not_truncated_or_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "original").unwrap();
        let ctx = ctx(dir.path()).with_path_verifier(Arc::new(|_paths| {
            Err("the opened path is not allowed".to_string())
        }));
        let error = WriteTool
            .execute(input("a.txt", "replacement"), ctx)
            .await
            .expect_err("the verifier refusal must fail the write");
        assert!(error.to_string().contains("not allowed"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }
}
