//! WebFetch tool — HTTP GET with HTML-to-markdown conversion.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Cache duration in seconds (15 minutes).
const CACHE_TTL_SECS: u64 = 900;

/// Cache entry for fetched URLs.
struct CacheEntry {
    content: String,
    fetched_at: std::time::Instant,
}

/// Simple URL cache to avoid redundant fetches.
#[derive(Default)]
pub struct WebFetchCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
}

impl WebFetchCache {
    pub async fn get(&self, url: &str) -> Option<String> {
        let map = self.entries.read().await;
        let entry = map.get(url)?;
        if entry.fetched_at.elapsed().as_secs() < CACHE_TTL_SECS {
            Some(entry.content.clone())
        } else {
            None
        }
    }

    pub async fn set(&self, url: String, content: String) {
        self.entries.write().await.insert(url, CacheEntry {
            content,
            fetched_at: std::time::Instant::now(),
        });
    }
}

/// Key for `ToolContext.extra` to access the shared cache.
pub const WEB_FETCH_CACHE_KEY: &str = "web_fetch_cache";

/// Fetch web pages and convert HTML to markdown.
#[derive(Debug)]
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "WebFetch".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["url", "prompt"],
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" },
                    "prompt": { "type": "string", "description": "What information to extract from the page" }
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

    async fn execute(&self, input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let url = extract_string(&input.arguments, "url")?;
        let prompt = extract_string(&input.arguments, "prompt")?;

        // Validate URL
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ToolError::InvalidInput {
                message: "URL must start with http:// or https://".into(),
            });
        }

        // Try cache first
        let cached = if let Some(cache) = get_cache(&_ctx) {
            cache.get(&url).await
        } else {
            None
        };

        let content = if let Some(cached) = cached {
            cached
        } else {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| ToolError::InvalidInput {
                    message: format!("failed to create HTTP client: {e}"),
                })?;

            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| ToolError::InvalidInput {
                    message: format!("failed to fetch URL: {e}"),
                })?;

            let status = resp.status();
            if !status.is_success() {
                return Err(ToolError::InvalidInput {
                    message: format!("HTTP {status} for {url}"),
                });
            }

            let html = resp.text().await.map_err(|e| ToolError::InvalidInput {
                message: format!("failed to read response body: {e}"),
            })?;

            let markdown = html_to_markdown(&html);

            // Cache the result
            if let Some(cache) = get_cache(&_ctx) {
                cache.set(url.clone(), markdown.clone()).await;
            }

            markdown
        };

        let output = format!(
            "Fetched: {url}\n\nExtracting: {prompt}\n\n---\n{content}"
        );

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }
}

fn get_cache(ctx: &ToolContext<'_>) -> Option<Arc<WebFetchCache>> {
    ctx.extra
        .get(WEB_FETCH_CACHE_KEY)
        .and_then(|v| v.downcast_ref::<Arc<WebFetchCache>>().map(Arc::clone))
}

/// Convert HTML to plain text/markdown.
fn html_to_markdown(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 80)
        .unwrap_or_else(|_| strip_tags_fallback(html))
}

fn strip_tags_fallback(html: &str) -> String {
    // Fallback: remove HTML tags, collapse whitespace
    let mut result = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(c);
        }
    }
    result
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"))
    }

    fn webfetch_input(url: &str, prompt: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "WebFetch".into(),
            arguments: serde_json::json!({ "url": url, "prompt": prompt }),
        }
    }

    #[tokio::test]
    async fn webfetch_invalid_url() {
        let result = WebFetchTool
            .execute(webfetch_input("not-a-url", "test"), test_ctx())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn webfetch_missing_required_field() {
        let input = ToolInput {
            call_id: "1".into(),
            name: "WebFetch".into(),
            arguments: serde_json::json!({ "url": "http://example.com" }),
        };
        let result = WebFetchTool.execute(input, test_ctx()).await;
        assert!(result.is_err());
    }

    #[test]
    fn html_to_markdown_strips_tags() {
        let html = "<p>Hello <b>world</b></p>";
        let result = html_to_markdown(html);
        assert!(result.contains("Hello"));
        assert!(result.contains("world"));
        assert!(!result.contains('<'));
    }

    #[tokio::test]
    async fn webfetch_cache_stores_result() {
        let cache = Arc::new(WebFetchCache::default());
        cache.set("http://test.com".into(), "cached content".into()).await;
        let got = cache.get("http://test.com").await;
        assert_eq!(got, Some("cached content".into()));
    }
}
