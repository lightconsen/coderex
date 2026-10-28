//! MCP result adapter — converts MCP `tools/call` JSON responses into `ToolResult`.

use coderex_core::types::{ImageBlock, ToolResult};
use serde_json::Value;

/// Largest image, in decoded bytes, that is forwarded to the model.
///
/// Everything above this is summarized as text instead: a huge base64 payload
/// burns context (and provider limits) without making the model any more able
/// to see it.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

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
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let content = mcp_result.get("content");
        let (output, images) = Self::extract_content(content);

        ToolResult {
            tool_call_id: tool_call_id.to_string(),
            output,
            is_error,
            images,
        }
    }

    /// Extract human-readable text and inline images from MCP content.
    fn extract_content(content: Option<&Value>) -> (String, Vec<ImageBlock>) {
        let Some(items) = content.and_then(|c| c.as_array()) else {
            return (
                content
                    .and_then(|c| c.as_str())
                    .map(String::from)
                    .unwrap_or_default(),
                Vec::new(),
            );
        };

        let mut parts = Vec::new();
        let mut images = Vec::new();
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
                    push_image(mime, data, &mut images)
                }
                Some("resource") => item.get("resource").map_or_else(
                    || "[Resource: unknown]".to_string(),
                    |resource| {
                        let uri = resource.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                        let text = resource.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        let blob = resource.get("blob").and_then(|b| b.as_str()).unwrap_or("");
                        let mime = resource
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            format!("[Resource: {uri}]\n{text}")
                        } else if !blob.is_empty() && mime.starts_with("image/") {
                            // An image resource is an image, whether it arrived
                            // as top-level image content or as a blob.
                            format!("[Resource: {uri}] {}", push_image(mime, blob, &mut images))
                        } else if !blob.is_empty() {
                            format!("[Resource: {uri}] (binary, {} bytes)", blob.len() * 3 / 4)
                        } else {
                            format!("[Resource: {uri}]")
                        }
                    },
                ),
                Some(other) => format!("[Unknown content type: {other}]"),
                None => item.to_string(),
            };
            if !item_text.is_empty() {
                parts.push(item_text);
            }
        }

        let output = if parts.is_empty() {
            "(no content)".to_string()
        } else {
            parts.join("\n\n")
        };
        (output, images)
    }
}

/// Push an image if it is small enough to forward, and return the text
/// placeholder that text-only consumers see.
fn push_image(mime: &str, data: &str, images: &mut Vec<ImageBlock>) -> String {
    if data.is_empty() {
        return format!("[image: {mime}, no data]");
    }
    let Some(data) = normalize_base64(data) else {
        return format!("[image: {mime}, invalid base64; omitted]");
    };
    let bytes = decoded_len(&data);
    if bytes > MAX_IMAGE_BYTES {
        return format!(
            "[image: {mime}, {} decoded] image skipped: exceeds the {} MiB limit",
            human_size(bytes),
            MAX_IMAGE_BYTES / (1024 * 1024)
        );
    }
    images.push(ImageBlock {
        media_type: mime.to_string(),
        data,
    });
    format!("[image: {mime}, {bytes} bytes decoded]")
}

/// Normalize a base64 payload, or say it is not one.
///
/// Strips a `data:…;base64,` prefix and whitespace, converts the URL-safe
/// alphabet to the standard one, and checks the shape. A provider rejects a
/// `tool_result` whose image data is not valid base64, so a malformed payload
/// is reported in text instead of being forwarded.
fn normalize_base64(data: &str) -> Option<String> {
    let data = data.trim();
    let data = data
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,").map(|(_, payload)| payload))
        .unwrap_or(data);
    let mut cleaned: String = data
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    {
        return None;
    }
    let without_padding = cleaned.trim_end_matches('=');
    if without_padding.len() % 4 == 1 {
        return None;
    }
    while !cleaned.len().is_multiple_of(4) {
        cleaned.push('=');
    }
    Some(cleaned)
}

/// How many bytes a base64 payload decodes to.
fn decoded_len(data: &str) -> usize {
    let padding = data.chars().rev().take_while(|c| *c == '=').count().min(2);
    (data.len().saturating_mul(3) / 4).saturating_sub(padding)
}

/// A size a person reads at a glance.
// A display ratio: byte counts are far below 2^53 in practice and the result is
// rounded to a single decimal place anyway.
#[allow(clippy::cast_precision_loss)]
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

        assert_eq!(tr.images.len(), 1);
        assert_eq!(tr.images[0].media_type, "image/png");
        assert_eq!(tr.images[0].data, "aGVsbG8=");
        // `aGVsbG8=` decodes to five bytes, so the size is the real one rather
        // than the length of the base64.
        assert!(
            tr.output.contains("[image: image/png, 5 bytes decoded]"),
            "{}",
            tr.output
        );
    }

    #[test]
    fn a_large_image_is_carried_out_of_band_not_inlined_in_text() {
        let data = "A".repeat(4_000);
        let result = serde_json::json!({
            "content": [{"type": "image", "data": data, "mimeType": "image/jpeg"}],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);

        assert_eq!(tr.images.len(), 1);
        assert_eq!(tr.images[0].media_type, "image/jpeg");
        assert_eq!(tr.images[0].data, data);
        assert!(tr.output.contains("image/jpeg"), "{}", tr.output);
        assert!(
            !tr.output.contains("AAAA"),
            "no base64 was inlined: {}",
            tr.output
        );
        assert!(tr.output.len() < 200, "{}", tr.output);
    }

    #[test]
    fn oversized_images_are_skipped_with_a_note() {
        // 7,000,000 base64 chars decode to 5,250,000 bytes, just over 5 MiB.
        let data = "A".repeat(7_000_000);
        let result = serde_json::json!({
            "content": [{"type": "image", "data": data, "mimeType": "image/png"}],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);

        assert!(tr.images.is_empty());
        assert!(tr.output.contains("image/png"), "{}", tr.output);
        assert!(tr.output.contains("skipped"), "{}", tr.output);
        assert!(tr.output.contains("5 MiB"), "{}", tr.output);
        assert!(
            !tr.output.contains("AAAA"),
            "no base64 was inlined: {}",
            tr.output
        );
    }

    #[test]
    fn image_base64_is_normalized_and_malformed_data_is_omitted() {
        let data_uri = serde_json::json!({
            "content": [{
                "type": "image",
                "data": "data:image/png;base64,QUJD",
                "mimeType": "image/png"
            }],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &data_uri);
        assert_eq!(tr.images.len(), 1);
        assert_eq!(tr.images[0].data, "QUJD", "the data URI prefix is stripped");

        let malformed = serde_json::json!({
            "content": [{"type": "image", "data": "not base64!!", "mimeType": "image/png"}],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-2", &malformed);
        assert!(tr.images.is_empty());
        assert!(tr.output.contains("invalid base64"), "{}", tr.output);
    }

    #[test]
    fn adapt_image_resource_blob() {
        let result = serde_json::json!({
            "content": [{
                "type": "resource",
                "resource": {
                    "uri": "file:///tmp/chart.png",
                    "blob": "aGVsbG8=",
                    "mimeType": "image/png"
                }
            }],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);

        assert_eq!(tr.images.len(), 1);
        assert_eq!(tr.images[0].media_type, "image/png");
        assert_eq!(tr.images[0].data, "aGVsbG8=");
        assert!(
            tr.output.contains("[Resource: file:///tmp/chart.png]"),
            "{}",
            tr.output
        );
        assert!(
            tr.output.contains("[image: image/png, 5 bytes decoded]"),
            "{}",
            tr.output
        );
    }

    #[test]
    fn non_image_resource_blob_keeps_size_only_text() {
        let result = serde_json::json!({
            "content": [{
                "type": "resource",
                "resource": {
                    "uri": "file:///tmp/data.bin",
                    "blob": "eyJvayI6dHJ1ZX0=",
                    "mimeType": "application/octet-stream"
                }
            }],
            "isError": false
        });
        let tr = McpResultAdapter::adapt("call-1", &result);

        assert!(tr.images.is_empty());
        assert!(
            tr.output
                .contains("[Resource: file:///tmp/data.bin] (binary, 12 bytes)"),
            "{}",
            tr.output
        );
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
