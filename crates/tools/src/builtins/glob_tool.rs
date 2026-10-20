//! Glob tool — file pattern matching.

use globset::GlobBuilder;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::path::PathBuf;

/// Maximum number of results to return.
const MAX_RESULTS: usize = 100;

/// Search for files matching a glob pattern.
#[derive(Debug)]
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Glob".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern to match" },
                    "path": { "type": "string", "description": "Root directory to search (default: cwd)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "matches": { "type": "array", "items": { "type": "string" } }
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
        use std::fmt::Write as _;

        let pattern = extract_string(&input.arguments, "pattern")?;
        let path = input
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(String::from);

        let root = path
            .as_ref()
            .map_or_else(|| ctx.cwd.to_path_buf(), PathBuf::from);

        let glob = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError::InvalidInput {
                message: format!("invalid glob pattern: {e}"),
            })?;
        let matcher = glob.compile_matcher();

        let mut matches = Vec::new();
        let walker = walkdir::WalkDir::new(&root).into_iter().filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                name != ".git"
            } else {
                true
            }
        });

        for entry in walker {
            let entry = entry.map_err(|e| ToolError::InvalidInput {
                message: format!("directory walk error: {e}"),
            })?;

            if entry.file_type().is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or_else(|_| entry.path());
                if matcher.is_match(relative) {
                    matches.push(relative.to_string_lossy().to_string());
                }
            }

            if matches.len() >= MAX_RESULTS {
                break;
            }
        }

        let mut output = matches.join("\n");
        if matches.len() >= MAX_RESULTS {
            let _ = write!(output, "\n\n... (truncated to {MAX_RESULTS} results)");
        }

        Ok(ToolOutput {
            output,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    fn glob_input_with_path(pattern: &str, path: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Glob".into(),
            arguments: serde_json::json!({ "pattern": pattern, "path": path }),
        }
    }

    #[tokio::test]
    async fn glob_match_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.rs"), "").unwrap();
        std::fs::write(tmp.path().join("main.rs"), "").unwrap();
        std::fs::write(tmp.path().join("readme.md"), "").unwrap();

        let output = GlobTool
            .execute(
                glob_input_with_path("*.rs", tmp.path().to_str().unwrap()),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("test.rs"));
        assert!(output.output.contains("main.rs"));
        assert!(!output.output.contains("readme.md"));
    }

    #[tokio::test]
    async fn glob_no_matches_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "").unwrap();

        let output = GlobTool
            .execute(
                glob_input_with_path("*.xyz", tmp.path().to_str().unwrap()),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.is_empty() || output.output.contains("truncated"));
    }

    #[tokio::test]
    async fn glob_nested_directory_match() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("lib.rs"), "").unwrap();

        let output = GlobTool
            .execute(
                glob_input_with_path("**/*.rs", tmp.path().to_str().unwrap()),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("lib.rs"));
    }

    #[tokio::test]
    async fn glob_excludes_dot_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), "").unwrap();
        std::fs::write(tmp.path().join("file.txt"), "").unwrap();

        let output = GlobTool
            .execute(
                glob_input_with_path("config", tmp.path().to_str().unwrap()),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        // .git/config should be skipped
        assert!(!output.output.contains(".git"));
    }

    #[tokio::test]
    async fn glob_result_limit() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..150 {
            std::fs::write(tmp.path().join(format!("file-{i}.txt")), "").unwrap();
        }

        let output = GlobTool
            .execute(
                glob_input_with_path("*.txt", tmp.path().to_str().unwrap()),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("truncated"));
        let line_count = output
            .output
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with("..."))
            .count();
        assert_eq!(line_count, MAX_RESULTS);
    }
}
