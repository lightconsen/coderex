//! WebFetch tool — fetch a URL and return its text body.
//!
//! This is a feature-gated tool: it is only offered when the session enables
//! the `web_browser_tool` feature flag (see
//! [`required_feature_flag`](crate::required_feature_flag)). The fetch is
//! deliberately narrow: absolute `http`/`https` URLs only, private and
//! loopback targets refused by [`SsrfGuard`], at most 1 MiB read from the
//! wire, and redirects refused rather than followed.

use std::time::Duration;

use coderex_core::hook_security::{SsrfGuard, UrlValidator};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Requests time out after this long.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How many bytes of the response body are read at most.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// How many characters of the body are returned by default.
const DEFAULT_MAX_CHARS: usize = 50_000;

/// The largest `max_chars` a caller may ask for.
const HARD_MAX_CHARS: usize = 200_000;

/// Fetch a URL over HTTP(S) and return its body as text.
#[derive(Debug)]
pub struct WebFetchTool {
    /// Whether to run the SSRF guard. Always true outside tests; the local
    /// test server is on loopback, which is exactly what the guard exists to
    /// refuse.
    enforce_ssrf: bool,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self { enforce_ssrf: true }
    }
}

impl WebFetchTool {
    /// Create a new WebFetch tool.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Skip the SSRF guard so a test can talk to a loopback server.
    #[cfg(test)]
    fn without_ssrf_guard(mut self) -> Self {
        self.enforce_ssrf = false;
        self
    }

    /// Reject anything that is not an absolute `http`/`https` URL pointing at
    /// a non-private host.
    async fn validate_url(&self, url: &str) -> Result<(), ToolError> {
        let parsed = reqwest::Url::parse(url).map_err(|e| ToolError::InvalidInput {
            message: format!("'{url}' is not an absolute URL: {e}"),
        })?;

        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ToolError::InvalidInput {
                message: format!(
                    "URL scheme '{}' is not allowed; only http and https are supported",
                    parsed.scheme()
                ),
            });
        }

        UrlValidator::new()
            .validate(url)
            .map_err(|e| ToolError::InvalidInput {
                message: format!("URL '{url}' was rejected: {e}"),
            })?;

        if self.enforce_ssrf {
            // The guard resolves the host with `ToSocketAddrs`, which blocks.
            // Keep the async runtime free while it runs.
            let owned = url.to_string();
            tokio::task::spawn_blocking(move || SsrfGuard::guard(&owned))
                .await
                .map_err(|e| ToolError::Execution {
                    name: "WebFetch".into(),
                    message: format!("SSRF check for '{url}' could not run: {e}"),
                })?
                .map_err(|e| ToolError::InvalidInput {
                    message: format!("URL '{url}' was rejected by the SSRF guard: {e}"),
                })?;
        }

        Ok(())
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "WebFetch".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http or https URL to fetch"
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Maximum body characters to return (default 50000, max 200000)"
                    }
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

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let url = extract_string(&input.arguments, "url")?;
        let max_chars = extract_max_chars(&input.arguments)?;

        self.validate_url(&url).await?;

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // A redirect would take the request somewhere the SSRF guard did
            // not check. Refuse it instead of following it.
            .redirect(Policy::none())
            .user_agent(format!("CodeRex/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ToolError::Execution {
                name: "WebFetch".into(),
                message: format!("failed to build HTTP client: {e}"),
            })?;

        let mut resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::Execution {
                name: "WebFetch".into(),
                message: format!("failed to fetch '{url}': {e}"),
            })?;

        let status = resp.status();

        if status.is_redirection() {
            let location = resp
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("<no Location header>");
            return Err(ToolError::Execution {
                name: "WebFetch".into(),
                message: format!(
                    "redirect not followed: HTTP {status} for '{url}' points to '{location}'"
                ),
            });
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<none>")
            .to_string();
        let final_url = resp.url().as_str().to_string();

        if !is_text_content_type(&content_type) {
            return Ok(ToolOutput {
                output: format!(
                    "Status: {status}\nContent-Type: {content_type}\nFinal URL: {final_url}\n\n\
                     content type '{content_type}' is not text; refusing to return the body"
                ),
                is_display: true,
            });
        }

        // Read at most `MAX_BODY_BYTES` from the wire; do not buffer an
        // arbitrarily large response just to truncate it afterwards.
        let mut body = Vec::new();
        let mut capped_by_size = false;
        loop {
            let chunk = resp.chunk().await.map_err(|e| ToolError::Execution {
                name: "WebFetch".into(),
                message: format!("failed to read the body of '{url}': {e}"),
            })?;
            let Some(chunk) = chunk else {
                break;
            };

            if body.len() + chunk.len() > MAX_BODY_BYTES {
                let remaining = MAX_BODY_BYTES - body.len();
                body.extend_from_slice(&chunk[..remaining]);
                capped_by_size = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }

        let body = String::from_utf8_lossy(&body);
        let char_count = body.chars().count();
        let truncated_by_chars = char_count > max_chars;
        let shown: String = body.chars().take(max_chars).collect();

        let mut output = format!(
            "Status: {status}\nContent-Type: {content_type}\nFinal URL: {final_url}\n\n{shown}"
        );
        if capped_by_size {
            output.push_str(&format!(
                "\n\n[body truncated at {MAX_BODY_BYTES} bytes while reading]"
            ));
        }
        if truncated_by_chars {
            output.push_str(&format!(
                "\n\n[body truncated at {max_chars} characters; {char_count} characters available]"
            ));
        }

        Ok(ToolOutput {
            output,
            is_display: true,
        })
    }
}

/// Whether a `Content-Type` describes text this tool is willing to return.
///
/// A missing header is treated as text: the server has not said the body is
/// binary, and refusing would turn an omission into a failure. A header that
/// names a non-text type is refused before the body is read.
fn is_text_content_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    if essence.is_empty() {
        return true;
    }

    essence.starts_with("text/")
        || matches!(
            essence.as_str(),
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/x-ndjson"
                | "application/ndjson"
                | "application/yaml"
                | "application/x-yaml"
                | "application/sql"
                | "application/graphql"
        )
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required string field: {key}"),
        })
}

fn extract_max_chars(args: &serde_json::Value) -> Result<usize, ToolError> {
    match args.get("max_chars") {
        None | Some(serde_json::Value::Null) => Ok(DEFAULT_MAX_CHARS),
        Some(value) => {
            let requested =
                value
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| ToolError::InvalidInput {
                        message: "max_chars must be a positive integer".into(),
                    })?;
            Ok(usize::try_from(requested)
                .unwrap_or(HARD_MAX_CHARS)
                .min(HARD_MAX_CHARS))
        }
    }
}

/// Convert HTML to plain text/markdown.
///
/// Used by the Chrome tool family to render page content; `WebFetch` returns
/// the body as-is so the caller sees exactly what the server sent.
pub fn html_to_markdown(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 80).unwrap_or_else(|_| strip_tags_fallback(html))
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
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use std::net::SocketAddr;
    use std::path::Path;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"))
    }

    fn webfetch_input(url: &str, max_chars: Option<u64>) -> ToolInput {
        let mut arguments = serde_json::json!({ "url": url });
        if let Some(max_chars) = max_chars {
            arguments["max_chars"] = serde_json::json!(max_chars);
        }
        ToolInput {
            call_id: "1".into(),
            name: "WebFetch".into(),
            arguments,
        }
    }

    /// Serve exactly one response, then close. The request is read first so
    /// the client has finished writing before the response arrives.
    async fn serve_once(response: Vec<u8>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });
        addr
    }

    fn response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn text_fetch_returns_status_type_url_and_body() {
        let addr = serve_once(response(
            "200 OK",
            "text/plain; charset=utf-8",
            "hello from the test server",
        ))
        .await;

        let tool = WebFetchTool::new().without_ssrf_guard();
        let output = tool
            .execute(
                webfetch_input(&format!("http://{addr}/page"), None),
                test_ctx(),
            )
            .await
            .unwrap();

        assert!(output.output.contains("200 OK"), "{}", output.output);
        assert!(output.output.contains("text/plain"), "{}", output.output);
        assert!(
            output.output.contains(&format!("http://{addr}/page")),
            "{}",
            output.output
        );
        assert!(
            output.output.contains("hello from the test server"),
            "{}",
            output.output
        );
        assert!(output.is_display);
    }

    #[tokio::test]
    async fn max_chars_truncates_the_body() {
        let addr = serve_once(response("200 OK", "text/plain", "abcdefghij")).await;

        let tool = WebFetchTool::new().without_ssrf_guard();
        let output = tool
            .execute(
                webfetch_input(&format!("http://{addr}/"), Some(5)),
                test_ctx(),
            )
            .await
            .unwrap();

        assert!(output.output.contains("abcde"), "{}", output.output);
        assert!(!output.output.contains("fghij"), "{}", output.output);
        assert!(
            output.output.contains("truncated at 5 characters"),
            "{}",
            output.output
        );
    }

    #[tokio::test]
    async fn max_chars_is_clamped_to_the_hard_maximum() {
        assert_eq!(
            extract_max_chars(&serde_json::json!({"max_chars": 1})).unwrap(),
            1
        );
        assert_eq!(
            extract_max_chars(&serde_json::json!({"max_chars": 10_000_000})).unwrap(),
            HARD_MAX_CHARS
        );
        assert_eq!(
            extract_max_chars(&serde_json::json!({})).unwrap(),
            DEFAULT_MAX_CHARS
        );
        assert!(extract_max_chars(&serde_json::json!({"max_chars": 0})).is_err());
        assert!(extract_max_chars(&serde_json::json!({"max_chars": -1})).is_err());
    }

    #[tokio::test]
    async fn non_text_content_type_is_refused() {
        let addr = serve_once(response(
            "200 OK",
            "image/png",
            "\u{89}PNG\r\n\u{1a}\n binary bytes",
        ))
        .await;

        let tool = WebFetchTool::new().without_ssrf_guard();
        let output = tool
            .execute(
                webfetch_input(&format!("http://{addr}/img.png"), None),
                test_ctx(),
            )
            .await
            .unwrap();

        assert!(
            output
                .output
                .contains("content type 'image/png' is not text"),
            "{}",
            output.output
        );
    }

    #[tokio::test]
    async fn redirects_are_refused_rather_than_followed() {
        let response = b"HTTP/1.1 302 Found\r\nLocation: http://example.com/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let addr = serve_once(response).await;

        let tool = WebFetchTool::new().without_ssrf_guard();
        let error = tool
            .execute(webfetch_input(&format!("http://{addr}/"), None), test_ctx())
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("redirect not followed"), "{message}");
        assert!(
            message.contains("http://example.com/elsewhere"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn loopback_urls_are_refused_by_the_ssrf_guard() {
        let tool = WebFetchTool::new();
        let error = tool
            .execute(
                webfetch_input("http://127.0.0.1:1/secret", None),
                test_ctx(),
            )
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("SSRF guard"), "{message}");
        assert!(message.contains("localhost"), "{message}");
    }

    #[tokio::test]
    async fn private_urls_are_refused_by_the_ssrf_guard() {
        let tool = WebFetchTool::new();
        let error = tool
            .execute(
                webfetch_input("http://192.168.1.10/secret", None),
                test_ctx(),
            )
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("SSRF guard"), "{message}");
        assert!(message.contains("private IP"), "{message}");
    }

    #[tokio::test]
    async fn oversized_bodies_are_capped_while_reading() {
        let body = "x".repeat(MAX_BODY_BYTES + 4096);
        let addr = serve_once(response("200 OK", "text/plain", &body)).await;

        let tool = WebFetchTool::new().without_ssrf_guard();
        let output = tool
            .execute(
                webfetch_input(&format!("http://{addr}/big"), Some(200_000)),
                test_ctx(),
            )
            .await
            .unwrap();

        assert!(
            output.output.contains("truncated at 1048576 bytes"),
            "{}",
            &output.output[..output.output.len().min(300)]
        );
        // And the requested character cap still applies on top of it.
        assert!(
            output.output.contains("truncated at 200000 characters"),
            "{}",
            &output.output[..output.output.len().min(300)]
        );
    }

    #[tokio::test]
    async fn non_http_scheme_is_refused() {
        let tool = WebFetchTool::new().without_ssrf_guard();
        let error = tool
            .execute(webfetch_input("file:///etc/passwd", None), test_ctx())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not allowed"), "{error}");

        let error = tool
            .execute(webfetch_input("/relative/path", None), test_ctx())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not an absolute URL"), "{error}");
    }

    #[tokio::test]
    async fn missing_url_is_an_error() {
        let tool = WebFetchTool::new().without_ssrf_guard();
        let input = ToolInput {
            call_id: "1".into(),
            name: "WebFetch".into(),
            arguments: serde_json::json!({}),
        };
        let error = tool.execute(input, test_ctx()).await.unwrap_err();
        assert!(error.to_string().contains("url"), "{error}");
    }

    #[test]
    fn text_content_types_are_recognised() {
        for value in [
            "text/plain",
            "text/html; charset=utf-8",
            "application/json",
            "application/ld+json",
            "application/xml",
            "application/javascript",
            "",
        ] {
            assert!(is_text_content_type(value), "{value} should be text");
        }
        for value in [
            "image/png",
            "application/octet-stream",
            "application/pdf",
            "video/mp4",
            "audio/mpeg",
        ] {
            assert!(!is_text_content_type(value), "{value} should not be text");
        }
    }

    #[test]
    fn html_to_markdown_strips_tags() {
        let html = "<p>Hello <b>world</b></p>";
        let result = html_to_markdown(html);
        assert!(result.contains("Hello"));
        assert!(result.contains("world"));
        assert!(!result.contains('<'));
    }
}
