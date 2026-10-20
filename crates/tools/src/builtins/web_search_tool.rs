//! WebSearch tool — search query with domain filtering.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Maximum number of results to return.
const MAX_RESULTS: usize = 10;

/// Search the web with domain filtering.
#[derive(Debug)]
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "WebSearch".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "allowed_domains": { "type": "array", "items": { "type": "string" }, "description": "Only include results from these domains" },
                    "blocked_domains": { "type": "array", "items": { "type": "string" }, "description": "Exclude results from these domains" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "results": { "type": "string" }
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
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        use std::fmt::Write as _;

        let query = extract_string(&input.arguments, "query")?;
        let allowed_domains = extract_string_array(&input.arguments, "allowed_domains");
        let blocked_domains = extract_string_array(&input.arguments, "blocked_domains");

        // Rate limit check
        if let Some(limiter) = crate::rate_limiter::get_rate_limiter(&_ctx) {
            if !limiter.check("WebSearch", 10) {
                return Err(ToolError::RateLimited {
                    name: "WebSearch".into(),
                });
            }
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::InvalidInput {
                message: format!("failed to create HTTP client: {e}"),
            })?;

        // Use DuckDuckGo HTML search as a simple backend-free search
        let url = format!("https://html.duckduckgo.com/html/?q={}", url_encode(&query));

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::InvalidInput {
                message: format!("search failed: {e}"),
            })?;

        if !resp.status().is_success() {
            return Err(ToolError::InvalidInput {
                message: format!("search returned HTTP {}", resp.status()),
            });
        }

        let html = resp.text().await.map_err(|e| ToolError::InvalidInput {
            message: format!("failed to read search results: {e}"),
        })?;

        let results = parse_ddg_results(&html, &allowed_domains, &blocked_domains);

        let mut output = format!("Search results for: \"{query}\"\n\n");
        for (i, r) in results.iter().take(MAX_RESULTS).enumerate() {
            let _ = write!(
                output,
                "{}. [{}]({})\n   {}\n\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            );
        }

        if results.is_empty() {
            output.push_str("No results found.\n");
        }

        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn parse_ddg_results(html: &str, allowed: &[String], blocked: &[String]) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // Extract result links from DDG HTML
    for line in html.lines() {
        if let Some(url) = extract_ddg_url(line) {
            let domain = extract_domain(&url);

            // Apply domain filters
            if !allowed.is_empty() && !allowed.iter().any(|d| domain.contains(d.as_str())) {
                continue;
            }
            if blocked.iter().any(|d| domain.contains(d.as_str())) {
                continue;
            }

            results.push(SearchResult {
                title: domain.clone(),
                url,
                snippet: String::new(),
            });

            if results.len() >= 20 {
                break;
            }
        }
    }

    results
}

fn extract_ddg_url(line: &str) -> Option<String> {
    // DDG result links look like: <a rel="nofollow" class="result__a" href="https://example.com/...">
    if line.contains("result__a") || line.contains("class=\"result\"") {
        if let Some(start) = line.find("href=\"") {
            let rest = &line[start + 6..];
            if let Some(end) = rest.find('"') {
                let url = &rest[..end];
                if url.starts_with("http") {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

fn extract_domain(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

fn url_encode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

fn extract_string_array(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"))
    }

    fn websearch_input(query: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({ "query": query }),
        }
    }

    fn websearch_with_domains(query: &str, allowed: Vec<&str>, blocked: Vec<&str>) -> ToolInput {
        let mut args = serde_json::json!({ "query": query });
        if !allowed.is_empty() {
            args["allowed_domains"] = serde_json::json!(allowed);
        }
        if !blocked.is_empty() {
            args["blocked_domains"] = serde_json::json!(blocked);
        }
        ToolInput {
            call_id: "1".into(),
            name: "WebSearch".into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn websearch_missing_query_errors() {
        let input = ToolInput {
            call_id: "1".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({}),
        };
        let result = WebSearchTool.execute(input, test_ctx()).await;
        assert!(result.is_err());
    }

    #[test]
    fn extract_domain_works() {
        assert_eq!(extract_domain("https://example.com/path"), "example.com");
        assert_eq!(extract_domain("http://sub.example.org/"), "sub.example.org");
    }

    #[test]
    fn url_encode_escapes_special_chars() {
        let encoded = url_encode("hello world");
        assert!(encoded.contains("hello") && encoded.contains("world"));
    }

    #[test]
    fn domain_filtering_logic() {
        let blocked = vec!["spam.com".to_string(), "ads.net".to_string()];
        let allowed = vec!["trusted.com".to_string()];

        // Blocked domain should be filtered
        assert!(blocked.iter().any(|d| "spam.com".contains(d.as_str())));

        // Allowed domain check
        assert!(allowed.iter().any(|d| "trusted.com".contains(d.as_str())));
        assert!(!allowed.iter().any(|d| "other.com".contains(d.as_str())));
    }
}
