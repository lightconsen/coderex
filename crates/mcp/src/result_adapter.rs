//! MCP result adapter — converts MCP `tools/call` JSON responses into `ToolResult`.

use coderex_core::types::ToolResult;
use serde_json::Value;

/// Adapter that converts MCP tool call results into engine-native `ToolResult`.
#[derive(Debug)]
pub struct McpResultAdapter;

impl McpResultAdapter {
    /// Convert an MCP `tools/call` response into a `ToolResult`.
    ///
    /// MCP result format:
    /// ```json
    /// {
    ///   "content": [
    ///     {"type": "text", "text": "..."},
    ///     {"type": "image", "data": "base64...", "mimeType": "image/png"},
    ///     {"type": "resource", "resource": {"uri": "...", "text": "..."}}
    ///   ],
    ///   "isError": false
    /// }
    /// ```
    pub fn adapt(tool_call_id: &str, mcp_result: &Value) -> ToolResult {
        let is_error = mcp_result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let content = mcp_result.get("content");
        let output = Self::extract_text(content);

        ToolResult {
            tool_call_id: tool_call_id.to_string(),
            output,
            is_error,
        }
    }

    /// Extract human-readable text from MCP content array.
    fn extract_text(content: Option<&Value>) -> String {
        let Some(items) = content.and_then(|c| c.as_array()) else {
            return content
                .and_then(|c| c.as_str())
                .map(String::from)
                .unwrap_or_default();
        };

        let mut parts = Vec::new();
        for item in items {
            let item_text = match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => item
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
                Some("image") => {
                    let mime = item
                        .get("mimeType")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/png");
                    let data = item.get("data").and_then(|d| d.as_str()).unwrap_or("");

                    // The tool result is text, so an image cannot travel in it
                    // and the model cannot see one. Sixty-four characters of
                    // base64 is not a smaller image — it is a broken one — so
                    // say what came back and how big it is instead of pretending
                    // to carry it. Carrying images needs content blocks the
                    // pipeline does not have.
                    format!(
                        "[Image: {mime}, {}] the image itself is not passed on: tool results \
                         are text",
                        human_size(decoded_len(data))
                    )
                }
                Some("resource") => {
                    if let Some(resource) = item.get("resource") {
                        let uri = resource.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                        let text = resource.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        let blob = resource.get("blob").and_then(|b| b.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            format!("[Resource: {uri}]\n{text}")
                        } else if !blob.is_empty() {
                            format!("[Resource: {uri}] (binary, {} bytes)", blob.len() * 3 / 4)
                        } else {
                            format!("[Resource: {uri}]")
                        }
                    } else {
                        "[Resource: unknown]".to_string()
                    }
                }
                Some(other) => format!("[Unknown content type: {other}]"),
                None => item.to_string(),
            };
            if !item_text.is_empty() {
                parts.push(item_text);
            }
        }

        if parts.is_empty() {
            "(no content)".to_string()
        } else {
            parts.join("\n\n")
        }
    }
}

/// How many bytes a base64 payload decodes to.
fn decoded_len(data: &str) -> usize {
    let padding = data.chars().rev().take_while(|c| *c == '=').count().min(2);
    data.len().saturating_mul(3) / 4 - padding
}

/// A size a person reads at a glance.
fn human_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapt_text_content() {
        let result = serde_json::json!({
            "content": [{"type": "text", "text": "hello world"}],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);
        assert!(!tr.is_error);
        assert_eq!(tr.output, "hello world");
    }

    #[test]
    fn adapt_multiple_text_blocks() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);
        assert_eq!(tr.output, "first\n\nsecond");
    }

    #[test]
    fn adapt_error_result() {
        let result = serde_json::json!({
            "content": [{"type": "text", "text": "something went wrong"}],
            "isError": true
        });
        let tr = McpResultAdapter::adapt("call-1", &result);
        assert!(tr.is_error);
        assert_eq!(tr.output, "something went wrong");
    }

    #[test]
    fn adapt_image_content() {
        let result = serde_json::json!({
            "content": [{"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);

        assert!(tr.output.contains("[Image: image/png"), "{}", tr.output);
        // `aGVsbG8=` decodes to five bytes, so the size is the real one rather
        // than the length of the base64.
        assert!(tr.output.contains("5 B"), "{}", tr.output);
        assert!(tr.output.contains("text"), "{}", tr.output);
    }

    #[test]
    fn an_image_larger_than_a_line_reports_its_size_not_a_prefix() {
        let data = "A".repeat(4_000);
        let result = serde_json::json!({
            "content": [{"type": "image", "data": data, "mimeType": "image/jpeg"}],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);

        assert!(tr.output.contains("image/jpeg"), "{}", tr.output);
        assert!(
            !tr.output.contains("AAAA"),
            "no base64 was inlined: {}",
            tr.output
        );
        assert!(tr.output.len() < 200, "{}", tr.output);
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(decoded_len(""), 0);
        assert_eq!(decoded_len("aGVsbG8="), 5);
        assert_eq!(decoded_len("AAAA"), 3);
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn adapt_resource_content() {
        let result = serde_json::json!({
            "content": [{
                "type": "resource",
                "resource": {"uri": "file:///tmp/test.txt", "text": "file contents"}
            }],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);
        assert!(tr.output.contains("file contents"));
        assert!(tr.output.contains("file:///tmp/test.txt"));
    }

    #[test]
    fn adapt_empty_content() {
        let result = serde_json::json!({"content": [], "isError": false});
        let tr = McpResultAdapter::adapt("call-1", &result);
        assert_eq!(tr.output, "(no content)");
    }

    #[test]
    fn adapt_missing_content() {
        let result = serde_json::json!({"isError": false});
        let tr = McpResultAdapter::adapt("call-1", &result);
        assert_eq!(tr.output, "");
    }
}
