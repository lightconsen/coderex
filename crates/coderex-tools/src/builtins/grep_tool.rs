//! Grep tool — regex content search across files.

use regex::Regex;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::path::PathBuf;

/// Maximum number of results to return.
const MAX_RESULTS: usize = 100;

/// Search file contents using regex patterns.
#[derive(Debug)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Grep".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for" },
                    "path": { "type": "string", "description": "Root directory to search (default: cwd)" },
                    "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Output format (default: content)" },
                    "glob": { "type": "string", "description": "Only search files matching this glob" },
                    "-C": { "type": "number", "description": "Context lines before and after" },
                    "-A": { "type": "number", "description": "Context lines after match" },
                    "-B": { "type": "number", "description": "Context lines before match" },
                    "head_limit": { "type": "number", "description": "Maximum number of results" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "matches": { "type": "string" }
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
        let pattern = extract_string(&input.arguments, "pattern")?;
        let path = input
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(String::from);
        let output_mode = extract_string_opt(&input.arguments, "output_mode").unwrap_or_else(|| "content".into());
        let glob_pattern = extract_string_opt(&input.arguments, "glob");
        let context = extract_number(&input.arguments, "-C").unwrap_or(0) as usize;
        let after = extract_number(&input.arguments, "-A").unwrap_or(context as i64) as usize;
        let before = extract_number(&input.arguments, "-B").unwrap_or(context as i64) as usize;
        let head_limit = extract_number(&input.arguments, "head_limit").unwrap_or(MAX_RESULTS as i64) as usize;

        let regex = Regex::new(&pattern).map_err(|e| ToolError::InvalidInput {
            message: format!("invalid regex pattern: {e}"),
        })?;

        let root = path
            .as_ref().map_or_else(|| ctx.cwd.to_path_buf(), PathBuf::from);

        let glob_matcher = glob_pattern.as_ref().map(|p| {
            globset::Glob::new(p)
                .map(|g| g.compile_matcher())
                .map_err(|e| ToolError::InvalidInput {
                    message: format!("invalid glob: {e}"),
                })
        }).transpose()?;

        let mut results = Vec::new();

        let walker = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_entry(|e| {
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    name != ".git"
                } else {
                    true
                }
            });

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            // Apply glob filter if specified
            if let Some(ref gm) = glob_matcher {
                if !gm.is_match(entry.path()) {
                    continue;
                }
            }

            let content = match std::fs::read_to_string(entry.path()) {
                Ok(c) => c,
                Err(_) => continue, // skip binary files
            };

            let lines: Vec<&str> = content.lines().collect();
            let mut file_matched = false;

            for (i, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    file_matched = true;
                    if matches!(output_mode.as_str(), "content") {
                        // Add context lines
                        let start = i.saturating_sub(before);
                        let end = (i + 1 + after).min(lines.len());

                        let rel = entry
                            .path()
                            .strip_prefix(&root)
                            .unwrap_or(entry.path());

                        for j in start..end {
                            let prefix = if j == i { ">" } else { " " };
                            results.push(format!("{}:{j}:{prefix} {}", rel.display(), lines[j]));
                        }
                    }
                }

                if head_limit > 0 && results.len() >= head_limit && output_mode == "content" {
                    break;
                }
            }

            if file_matched && matches!(output_mode.as_str(), "files_with_matches") {
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path());
                results.push(rel.display().to_string());
            }

            if file_matched && matches!(output_mode.as_str(), "count") {
                let rel = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path());
                let count = regex.find_iter(&content).count();
                results.push(format!("{}:{count}", rel.display()));
            }

            if results.len() >= head_limit && !matches!(output_mode.as_str(), "content") {
                break;
            }
        }

        let mut output = results.join("\n");
        if head_limit > 0 && results.len() >= head_limit {
            output.push_str(&format!("\n\n... (truncated to {head_limit} results)"));
        }

        Ok(ToolOutput {
            output,
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

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn extract_number(args: &serde_json::Value, key: &str) -> Option<i64> {
    args.get(key).and_then(serde_json::Value::as_i64)
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

    fn grep_input(pattern: &str, args: serde_json::Value) -> ToolInput {
        let mut map = serde_json::Map::new();
        map.insert("pattern".into(), serde_json::json!(pattern));
        if let serde_json::Value::Object(extra) = args {
            for (k, v) in extra {
                map.insert(k, v);
            }
        }
        ToolInput {
            call_id: "1".into(),
            name: "Grep".into(),
            arguments: serde_json::Value::Object(map),
        }
    }

    #[tokio::test]
    async fn grep_pattern_match_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello world\nfoo bar\nhello again").unwrap();

        let output = GrepTool
            .execute(
                grep_input("hello", serde_json::json!({ "path": tmp.path().to_str().unwrap() })),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("hello world"));
        assert!(output.output.contains("hello again"));
    }

    #[tokio::test]
    async fn grep_files_with_matches_mode() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match here").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "nothing here").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "another match").unwrap();

        let output = GrepTool
            .execute(
                grep_input("match", serde_json::json!({
                    "path": tmp.path().to_str().unwrap(),
                    "output_mode": "files_with_matches"
                })),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("a.txt"));
        assert!(output.output.contains("c.txt"));
        assert!(!output.output.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_count_mode() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "x\nx\ny\nx").unwrap();

        let output = GrepTool
            .execute(
                grep_input("x", serde_json::json!({
                    "path": tmp.path().to_str().unwrap(),
                    "output_mode": "count"
                })),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains(":3"));
    }

    #[tokio::test]
    async fn grep_context_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "line1\nline2\nTARGET\nline4\nline5";
        std::fs::write(tmp.path().join("test.txt"), content).unwrap();

        let output = GrepTool
            .execute(
                grep_input("TARGET", serde_json::json!({
                    "path": tmp.path().to_str().unwrap(),
                    "-B": 1,
                    "-A": 1
                })),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("line2"));
        assert!(output.output.contains("TARGET"));
        assert!(output.output.contains("line4"));
    }

    #[tokio::test]
    async fn grep_head_limit_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (0..50).map(|i| format!("line {i} test")).collect::<Vec<_>>().join("\n");
        std::fs::write(tmp.path().join("test.txt"), &content).unwrap();

        let output = GrepTool
            .execute(
                grep_input("test", serde_json::json!({
                    "path": tmp.path().to_str().unwrap(),
                    "head_limit": 5
                })),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("truncated"));
        let match_lines: Vec<_> = output.output.lines().filter(|l| !l.is_empty() && !l.starts_with("...")).collect();
        assert_eq!(match_lines.len(), 5);
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello world").unwrap();

        let output = GrepTool
            .execute(
                grep_input("notfound", serde_json::json!({
                    "path": tmp.path().to_str().unwrap(),
                })),
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.output.is_empty());
    }
}
