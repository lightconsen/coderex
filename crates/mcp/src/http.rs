//! HTTP and SSE JSON-RPC transport for remote MCP servers.

use crate::channel::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::collections::HashMap;

/// A remote JSON-RPC channel. Requests always use POST; responses may be a
/// single JSON document or an SSE stream containing JSON-RPC messages.
#[derive(Debug)]
pub struct HttpChannel {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
}

impl HttpChannel {
    pub fn new(url: &str, headers: HashMap<String, String>) -> Result<Self, String> {
        let url = Url::parse(url).map_err(|error| format!("invalid MCP server URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!(
                "unsupported MCP server URL scheme '{}'; expected http or https",
                url.scheme()
            ));
        }

        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| format!("invalid HTTP header name: {error}"))?;
            let value = HeaderValue::from_str(&value)
                .map_err(|error| format!("invalid value for HTTP header {name}: {error}"))?;
            header_map.insert(name, value);
        }

        Ok(Self {
            client: reqwest::Client::new(),
            url,
            headers: header_map,
        })
    }

    pub async fn send_request(&self, request: &JsonRpcRequest) -> Result<Value, String> {
        let response = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .json(request)
            .send()
            .await
            .map_err(|error| format!("MCP HTTP request failed: {error}"))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let body = response
            .text()
            .await
            .map_err(|error| format!("failed to read MCP HTTP response: {error}"))?;

        if !status.is_success() {
            return Err(format!("MCP HTTP server returned {status}: {body}"));
        }
        if body.trim().is_empty() {
            return Err(format!(
                "MCP HTTP server returned an empty response to request {}",
                request.id
            ));
        }

        let response = if content_type.contains("text/event-stream") {
            parse_sse_response(&body, request.id)?
        } else {
            serde_json::from_str(&body)
                .map_err(|error| format!("failed to parse MCP HTTP response: {error}"))?
        };

        if let Some(error) = response.error {
            return Err(format!(
                "JSON-RPC error (code {}): {}",
                error.code, error.message
            ));
        }
        Ok(response.result.unwrap_or(Value::Null))
    }

    pub async fn send_notification(
        &self,
        notification: &JsonRpcNotification,
    ) -> Result<(), String> {
        let response = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .json(notification)
            .send()
            .await
            .map_err(|error| format!("MCP HTTP notification failed: {error}"))?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(format!(
                "MCP HTTP server returned {status} for a notification: {body}"
            ))
        }
    }
}

/// Find the JSON-RPC response with `expected_id` in an SSE document.
fn parse_sse_response(body: &str, expected_id: u64) -> Result<JsonRpcResponse, String> {
    let normalized = body.replace("\r\n", "\n");
    for event in normalized.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }

        let response: JsonRpcResponse = match serde_json::from_str(&data) {
            Ok(response) => response,
            Err(_) => continue,
        };
        if response.id == expected_id {
            return Ok(response);
        }
    }

    Err(format!(
        "SSE stream ended before a response to request {expected_id}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_rpc_response_from_sse() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tools\":[]}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = parse_sse_response(body, 7).unwrap();
        assert_eq!(response.id, 7);
        assert!(response.result.is_some());
    }

    #[test]
    fn skips_notifications_and_multiline_data() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":9,\n",
            "data: \"result\":{\"ok\":true}}\n\n"
        );
        let response = parse_sse_response(body, 9).unwrap();
        assert_eq!(response.result.unwrap()["ok"], true);
    }

    #[test]
    fn rejects_invalid_urls_and_headers() {
        assert!(HttpChannel::new("file:///tmp/mcp", HashMap::new()).is_err());
        let headers = HashMap::from([("bad header".into(), "value".into())]);
        assert!(HttpChannel::new("http://localhost/mcp", headers).is_err());
    }
}
