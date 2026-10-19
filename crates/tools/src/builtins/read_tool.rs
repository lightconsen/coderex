//! Read tool — read file contents (text, image, PDF).

use std::path::PathBuf;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{
    MaxResultSizeChars, RuleContentField, Tool, ToolCapabilities, ToolContext, ToolDescriptor,
    ToolInput, ToolTier,
};
use async_trait::async_trait;
use serde_json::Value;

/// Image file extensions that should be returned as base64.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico"];

/// Maximum number of lines to read by default.
const DEFAULT_LINE_LIMIT: usize = 2000;

/// Maximum number of characters to display before truncation.
const MAX_DISPLAY_CHARS: usize = 10000;

/// Largest image forwarded to the model as an image block (5 MiB decoded).
const MAX_FORWARDED_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Read file contents — text, images as base64, or PDF text extraction.
#[derive(Debug)]
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Read".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["file_path"],
                "properties": {
                    "file_path": { "type": "string" },
                    "offset": { "type": "number", "description": "Skip this many lines from the start" },
                    "limit": { "type": "number", "description": "Maximum number of lines to read (default 2000)" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            // Read is the one tool that declares "no cap": it bounds its own
            // output (MAX_DISPLAY_CHARS) before returning, so a cap here would
            // only ever cut a string the tool already decided to hand over.
            max_result_size_chars: MaxResultSizeChars::Unlimited,
            // The argument a path rule is about, named by the tool rather than
            // guessed from the shape of the call.
            rule_content_field: Some(RuleContentField::Scalar("file_path")),
            ..ToolCapabilities::default()
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let file_path = extract_file_path(&input.arguments)?;
        let offset = extract_number(&input.arguments, "offset").unwrap_or(0) as usize;
        let limit =
            extract_number(&input.arguments, "limit").unwrap_or(DEFAULT_LINE_LIMIT as i64) as usize;

        let path = resolve_path(&file_path, ctx.cwd);
        let path_buf = PathBuf::from(&path);

        if !path_buf.exists() {
            return Err(ToolError::InvalidInput {
                message: format!("file not found: {path}"),
            });
        }

        let ext = path_buf
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Image handling
        if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            return read_image(&path_buf, &ext);
        }

        // Text file handling
        let content = std::fs::read_to_string(&path_buf).map_err(|e| ToolError::InvalidInput {
            message: format!("failed to read file: {e}"),
        })?;

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.min(lines.len());
        let end = (start + limit).min(lines.len());
        let excerpt = lines[start..end].join("\n");

        if excerpt.len() > MAX_DISPLAY_CHARS {
            Ok(ToolOutput {
                output: format!(
                    "{}\n\n... (truncated, {} chars omitted)",
                    &excerpt[..MAX_DISPLAY_CHARS],
                    excerpt.len() - MAX_DISPLAY_CHARS
                ),
                is_display: true,
                images: Vec::new(),
            })
        } else {
            Ok(ToolOutput {
                output: excerpt,
                is_display: true,
                images: Vec::new(),
            })
        }
    }
}

fn extract_file_path(args: &Value) -> Result<String, ToolError> {
    args.get("file_path")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: "missing required field: file_path".into(),
        })
}

fn extract_number(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn resolve_path(file_path: &str, cwd: &std::path::Path) -> String {
    let path = PathBuf::from(file_path);
    if path.is_absolute() {
        // Some providers occasionally emit repo-relative paths with a leading
        // slash (e.g. "/src/lib.rs"). If that absolute path does not exist but
        // `<cwd>/<trimmed>` does, prefer the workspace-local file.
        if !path.exists() {
            if let Ok(trimmed) = path.strip_prefix("/") {
                let candidate = cwd.join(trimmed);
                if candidate.exists() {
                    return candidate.to_string_lossy().to_string();
                }
            }
        }
        file_path.to_string()
    } else {
        cwd.join(&path).to_string_lossy().to_string()
    }
}

fn read_image(path: &PathBuf, ext: &str) -> Result<ToolOutput, ToolError> {
    let bytes = std::fs::read(path).map_err(|e| ToolError::InvalidInput {
        message: format!("failed to read image: {e}"),
    })?;

    let mime = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    };

    // The provider accepts these formats as image blocks. SVG/BMP/ICO have no
    // supported media type there, so they stay inline text.
    let forwardable = matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    );
    if forwardable && bytes.len() <= MAX_FORWARDED_IMAGE_BYTES {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        return Ok(ToolOutput {
            output: format!("[image: {mime}, {} bytes]", bytes.len()),
            is_display: true,
            images: vec![coderex_core::types::ImageBlock {
                media_type: mime.to_string(),
                data: b64,
            }],
        });
    }
    if forwardable {
        return Ok(ToolOutput {
            output: format!(
                "[image: {mime}, {} bytes] not forwarded: exceeds the {} MiB image limit",
                bytes.len(),
                MAX_FORWARDED_IMAGE_BYTES / (1024 * 1024)
            ),
            is_display: true,
            images: Vec::new(),
        });
    }

    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);

    Ok(ToolOutput {
        output: format!("<image>data:{mime};base64,{b64}</image>"),
        is_display: true,
        images: Vec::new(),
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

    fn test_input(file_path: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Read".into(),
            arguments: serde_json::json!({ "file_path": file_path }),
        }
    }

    #[tokio::test]
    async fn read_text_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "hello world\nline two\nline three").unwrap();

        let output = ReadTool
            .execute(test_input("test.txt"), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert!(output.output.contains("hello world"));
        assert!(output.is_display);
    }

    #[tokio::test]
    async fn read_nonexistent_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let result = ReadTool
            .execute(test_input("no_such_file.txt"), test_ctx(tmp.path()))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn resolve_path_maps_leading_slash_repo_relative() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn add() {}").unwrap();

        let resolved = resolve_path("/src/lib.rs", tmp.path());
        assert_eq!(resolved, tmp.path().join("src/lib.rs").to_string_lossy());
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (0..10)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("lines.txt"), &content).unwrap();

        let output = ReadTool
            .execute(
                ToolInput {
                    call_id: "2".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({ "file_path": "lines.txt", "offset": 3, "limit": 2 }),
                },
                test_ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "line-3\nline-4");
    }

    #[tokio::test]
    async fn read_image_returns_base64() {
        let tmp = tempfile::tempdir().unwrap();
        // Minimal PNG header (fake but valid extension)
        let png_data: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        std::fs::write(tmp.path().join("img.png"), &png_data).unwrap();

        let output = ReadTool
            .execute(test_input("img.png"), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(output.images.len(), 1, "the image was not forwarded");
        assert_eq!(output.images[0].media_type, "image/png");
        assert!(
            output.output.starts_with("[image: image/png,"),
            "{}",
            output.output
        );
    }

    #[tokio::test]
    async fn read_large_file_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "x".repeat(MAX_DISPLAY_CHARS + 500);
        std::fs::write(tmp.path().join("large.txt"), &content).unwrap();

        let output = ReadTool
            .execute(test_input("large.txt"), test_ctx(tmp.path()))
            .await
            .unwrap();
        assert!(output.output.contains("truncated"));
        assert!(output.output.len() < content.len() + 100); // some overhead for truncation message
    }
}
