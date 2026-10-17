//! HTTP and SSE JSON-RPC transport for remote MCP servers.

use crate::channel::{
    InboundRequestHandler, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    JsonRpcServerRequest, METHOD_NOT_FOUND,
};
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

    /// Send a request and wait for its response.
    ///
    /// The protocol runs both ways over streamable HTTP too: an SSE response
    /// stream may carry a request from the server (elicitation is the common
    /// one) before the response to our request. Those are answered as they
    /// arrive — with a separate POST, since the stream is still open and the
    /// server may be waiting — so a server that asks a question is never left
    /// waiting for an answer that is not coming.
    pub async fn send_request(
        &self,
        request: &JsonRpcRequest,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<Value, String> {
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

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("MCP HTTP server returned {status}: {body}"));
        }

        if content_type.contains("text/event-stream") {
            return self.read_sse_response(response, request.id, handler).await;
        }

        let body = response
            .text()
            .await
            .map_err(|error| format!("failed to read MCP HTTP response: {error}"))?;
        if body.trim().is_empty() {
            return Err(format!(
                "MCP HTTP server returned an empty response to request {}",
                request.id
            ));
        }

        let value: Value = serde_json::from_str(&body)
            .map_err(|error| format!("failed to parse MCP HTTP response: {error}"))?;
        self.route_message(&value, request.id, handler)
            .await?
            .ok_or_else(|| {
                format!(
                    "MCP HTTP response to request {} contained no response to it",
                    request.id
                )
            })
    }

    /// Read an SSE response stream, answering server requests as they arrive.
    ///
    /// Read incrementally rather than buffering the whole body: a server that
    /// asks a question mid-stream may hold its own response until the answer
    /// arrives, so waiting for the end of the body would deadlock.
    async fn read_sse_response(
        &self,
        mut response: reqwest::Response,
        expected_id: u64,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<Value, String> {
        // Bytes, not `String`: a network chunk may split a multi-byte UTF-8
        // character, and decoding each chunk on its own would replace half of
        // it with U+FFFD. Event boundaries are ASCII, so a complete event is
        // always valid UTF-8 and the partial tail can stay undecoded.
        let mut buffer = Vec::new();

        loop {
            for event in take_events(&mut buffer) {
                if let Some(result) = self.route_sse_event(&event, expected_id, handler).await? {
                    return Ok(result);
                }
            }

            match response.chunk().await {
                Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(error) => return Err(format!("failed to read MCP SSE response: {error}")),
            }
        }

        // A stream may end without the blank line that terminates its last
        // event; the data that arrived is still a message.
        if !buffer.is_empty() {
            let event = String::from_utf8_lossy(&buffer).into_owned();
            buffer.clear();
            if let Some(result) = self.route_sse_event(&event, expected_id, handler).await? {
                return Ok(result);
            }
        }

        Err(format!(
            "SSE stream ended before a response to request {expected_id}"
        ))
    }

    /// Parse one SSE event and route the JSON-RPC message it carries.
    async fn route_sse_event(
        &self,
        event: &str,
        expected_id: u64,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<Option<Value>, String> {
        let Some(data) = sse_event_data(event) else {
            return Ok(None);
        };
        if data.trim() == "[DONE]" {
            return Ok(None);
        }

        // Not every SSE `data:` field is a JSON-RPC message (servers may send
        // keep-alives); skip what does not parse rather than failing the call.
        let value: Value = match serde_json::from_str(&data) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        self.route_message(&value, expected_id, handler).await
    }

    /// Tell a response, a notification, and a server request apart by shape.
    ///
    /// Returns `Some(result)` only when the response to `expected_id` arrives.
    async fn route_message(
        &self,
        value: &Value,
        expected_id: u64,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<Option<Value>, String> {
        let has_method = value.get("method").is_some();
        let has_id = value.get("id").is_some();

        match (has_method, has_id) {
            // The server asking us to do something, and waiting for the answer.
            (true, true) => {
                let request: JsonRpcServerRequest = serde_json::from_value(value.clone())
                    .map_err(|error| format!("failed to parse a server request: {error}"))?;
                self.answer_server_request(&request, handler).await?;
                Ok(None)
            }
            // A notification: no answer is expected.
            (true, false) => {
                tracing::debug!(
                    message = %value,
                    "ignoring a notification from the server"
                );
                Ok(None)
            }
            (false, true) => {
                let response: JsonRpcResponse = serde_json::from_value(value.clone())
                    .map_err(|error| format!("failed to parse JSON-RPC response: {error}"))?;

                if response.id != expected_id {
                    // An answer to a request that is no longer being waited on.
                    tracing::debug!(
                        id = response.id,
                        expected = expected_id,
                        "a response arrived for a request nothing is waiting on"
                    );
                    return Ok(None);
                }
                if let Some(error) = response.error {
                    return Err(format!(
                        "JSON-RPC error (code {}): {}",
                        error.code, error.message
                    ));
                }
                Ok(Some(response.result.unwrap_or(Value::Null)))
            }
            // Neither shape: say so rather than read past it.
            (false, false) => {
                tracing::warn!(
                    message = %value,
                    "a message from the server is neither a response nor a request"
                );
                Ok(None)
            }
        }
    }

    /// Answer a request the server made, over a new POST.
    async fn answer_server_request(
        &self,
        request: &JsonRpcServerRequest,
        handler: Option<&dyn InboundRequestHandler>,
    ) -> Result<(), String> {
        let answer = handler.map_or_else(
            || {
                Err(JsonRpcError {
                    code: METHOD_NOT_FOUND,
                    message: format!(
                        "this client has no handler for '{}'; nothing here can answer it",
                        request.method
                    ),
                    data: None,
                })
            },
            |handler| handler.handle(&request.method, request.params.as_ref()),
        );

        let response = match answer {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        };

        let delivered = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .json(&response)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "failed to answer the server's '{}' request: {error}",
                    request.method
                )
            })?;

        let status = delivered.status();
        if !status.is_success() {
            let body = delivered.text().await.unwrap_or_default();
            return Err(format!(
                "the server rejected the answer to '{}' with {status}: {body}",
                request.method
            ));
        }
        Ok(())
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

/// Drain every complete SSE event from `buffer`, leaving any partial one.
///
/// An event ends at a blank line; the three line-ending spellings the SSE
/// grammar allows are all accepted. Separators are found on the raw bytes, so
/// one split across two chunks is still recognised once the rest arrives.
fn take_events(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut events = Vec::new();
    while let Some((index, separator_len)) = find_event_boundary(buffer.as_slice()) {
        events.push(String::from_utf8_lossy(&buffer[..index]).into_owned());
        buffer.drain(..index + separator_len);
    }
    events
}

/// Find the earliest `\n\n`, `\r\n\r\n`, or `\r\r` in `bytes`.
fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if bytes[index] == b'\n' && bytes[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
        if bytes[index] == b'\r' && bytes[index..].starts_with(b"\r\r") {
            return Some((index, 2));
        }
        index += 1;
    }
    None
}

/// The `data:` payload of one SSE event, with multi-line data joined by `\n`.
fn sse_event_data(event: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in event.split(['\n', '\r']) {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        data.push(rest.strip_prefix(' ').unwrap_or(rest));
    }

    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(body: &str) -> Vec<String> {
        let mut buffer = body.as_bytes().to_vec();
        let events = take_events(&mut buffer);
        assert!(buffer.is_empty(), "left a partial event: {buffer:?}");
        events
    }

    #[test]
    fn splits_sse_events_and_keeps_the_data_payload() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tools\":[]}}\n\n",
            "data: [DONE]\n\n"
        );
        let events = events(body);
        assert_eq!(events.len(), 2);
        assert_eq!(
            sse_event_data(&events[0]).as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#)
        );
        assert_eq!(sse_event_data(&events[1]).as_deref(), Some("[DONE]"));
    }

    #[test]
    fn joins_multiline_data_and_accepts_other_line_endings() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\r\n\r\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":9,\r\n",
            "data: \"result\":{\"ok\":true}}\r\r"
        );
        let events = events(body);
        assert_eq!(events.len(), 2);
        assert_eq!(
            sse_event_data(&events[1]).as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"id\":9,\n\"result\":{\"ok\":true}}")
        );
    }

    #[test]
    fn a_partial_event_stays_in_the_buffer_for_the_next_chunk() {
        let mut buffer = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,".to_vec();
        assert!(take_events(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\"result\":{}}\n\n");
        let events = take_events(&mut buffer);
        assert_eq!(events.len(), 1);
        assert_eq!(
            sse_event_data(&events[0]).as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
        );
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_is_not_corrupted() {
        // "é" is two bytes; split them the way the network can.
        let mut buffer = b"data: {\"text\":\"caf".to_vec();
        buffer.push(0xC3);
        assert!(take_events(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\xA9\"}\n\n");
        let events = take_events(&mut buffer);
        assert_eq!(events.len(), 1);
        assert_eq!(
            sse_event_data(&events[0]).as_deref(),
            Some("{\"text\":\"café\"}")
        );
    }

    #[test]
    fn rejects_invalid_urls_and_headers() {
        assert!(HttpChannel::new("file:///tmp/mcp", HashMap::new()).is_err());
        let headers = HashMap::from([("bad header".into(), "value".into())]);
        assert!(HttpChannel::new("http://localhost/mcp", headers).is_err());
    }
}
