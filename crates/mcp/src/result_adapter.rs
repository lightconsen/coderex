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
                    let data = item
                        .get("data")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    let truncated = if data.len() > 64 {
                        format!("{}...[truncated]", &data[..64])
                    } else {
                        data.to_string()
                    };
                    format!("[Image: {mime}] {truncated}")
                }
                Some("resource") => {
                    if let Some(resource) = item.get("resource") {
                        let uri = resource
                            .get("uri")
                            .and_then(|u| u.as_str())
                            .unwrap_or("");
                        let text = resource
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let blob = resource
                            .get("blob")
                            .and_then(|b| b.as_str())
                            .unwrap_or("");
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
        assert!(tr.output.contains("[Image: image/png]"));
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
