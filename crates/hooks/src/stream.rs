//! The line-delimited protocol a hook speaks on stdout while it runs.
//!
//! The historical contract was one line: a hook printed one JSON object and
//! that object was its answer. That is still valid, and still the last word.
//! The streaming protocol adds messages a hook may emit *before* the answer:
//!
//! - `{"type":"log","message":"..."}` — a note for the operator. Not an answer.
//! - `{"type":"event","event":"name","data":...}` — progress. Collected and
//!   logged; not an answer.
//! - `{"type":"request","id":1,"method":"...","params":...}` — the hook asks
//!   the host something and waits. The host answers on the hook's stdin with
//!   `{"type":"response","id":1,"result":...}`, or an `error` object when no
//!   handler is installed.
//! - a JSON object without one of those `type`s — the final [`HookResult`].
//!
//! The host may send `{"type":"cancel","reason":"timeout"}` before killing a
//! hook that has run past its deadline. Cancellation is best effort: the
//! process is killed either way.
//!
//! The first line on the hook's stdin is the [`HookInput`](crate::HookInput),
//! unchanged from the one-way contract. Host messages follow it, and only
//! when the hook sets `stream: true`; otherwise stdin is closed after the
//! input so a hook that reads to EOF still sees the end.
//!
//! Every message is one line, so a hook written in any language can produce
//! it. A line that is not JSON, or a control message missing the field its
//! type requires, is a failure rather than a skipped line: silence is not
//! consent, and neither is a malformed answer.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// A progress message a hook emitted before its answer.
#[derive(Debug, Clone, PartialEq)]
pub struct HookStreamEvent {
    /// The event name the hook chose.
    pub event: String,
    /// Optional structured payload.
    pub data: Option<Value>,
}

/// A request a hook makes of the host while it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct HookHostRequest {
    /// The id the answer must carry back.
    pub id: u64,
    /// The method the host is asked to perform.
    pub method: String,
    /// Optional parameters.
    pub params: Option<Value>,
}

/// Why the host could not answer a hook's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRequestError {
    /// JSON-RPC-style error code.
    pub code: i64,
    /// Human-readable reason.
    pub message: String,
}

/// JSON-RPC's "no such method" error code, used when no handler is installed.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// Answers requests a hook makes of the session while it runs.
///
/// The hooks crate cannot reach a model client or the tool registry, so the
/// session that owns them may install one implementation. Without a handler a
/// request is answered with [`METHOD_NOT_FOUND`] — refused, never ignored: the
/// hook is waiting for the answer.
pub trait HookRequestHandler: Send + Sync + std::fmt::Debug {
    /// Answer one request, or say why it cannot be answered.
    fn handle(&self, method: &str, params: Option<&Value>) -> Result<Value, HookRequestError>;
}

/// One parsed stdout message.
pub(crate) enum HookMessage {
    /// Progress, collected by the caller.
    Event(HookStreamEvent),
    /// A note for the log.
    Log(String),
    /// A question for the host.
    Request(HookHostRequest),
    /// The final answer. The caller parses the raw line; the variant only says
    /// that this line is the answer rather than a control message.
    Result,
}

/// Parse one line of hook stdout.
///
/// `Ok(None)` is a blank line. Any other line must be a JSON object; a
/// control message must carry the fields its type names. A line without a
/// recognised control `type` is the final answer, which keeps the historical
/// one-line contract working unchanged.
pub(crate) fn parse_message(line: &str) -> Result<Option<HookMessage>, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let value: Value = serde_json::from_str(trimmed)
        .map_err(|error| format!("hook output line is not JSON: {error}"))?;

    match value.get("type").and_then(Value::as_str) {
        Some("event") => {
            let event = value
                .get("event")
                .and_then(Value::as_str)
                .ok_or_else(|| "an event message has no 'event' string".to_string())?;
            Ok(Some(HookMessage::Event(HookStreamEvent {
                event: event.to_string(),
                data: value.get("data").cloned(),
            })))
        }
        Some("log") => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.get("data").and_then(Value::as_str))
                .ok_or_else(|| "a log message has no 'message' string".to_string())?;
            Ok(Some(HookMessage::Log(message.to_string())))
        }
        Some("request") => {
            let id = value
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| "a request message has no numeric 'id'".to_string())?;
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| "a request message has no 'method' string".to_string())?;
            Ok(Some(HookMessage::Request(HookHostRequest {
                id,
                method: method.to_string(),
                params: value.get("params").cloned(),
            })))
        }
        // Everything else is the answer, including an explicit
        // `"type":"result"` and the historical bare object.
        _ => Ok(Some(HookMessage::Result)),
    }
}

/// The line the host writes back for a hook's request.
///
/// Serialization cannot fail for a `Value`, so this returns a `String` rather
/// than a `Result`; a request always gets an answer.
pub(crate) fn response_line(
    request: &HookHostRequest,
    handler: Option<&dyn HookRequestHandler>,
) -> String {
    match handler.map_or_else(
        || {
            Err(HookRequestError {
                code: METHOD_NOT_FOUND,
                message: format!(
                    "this session has no handler for '{}'; nothing here can answer it",
                    request.method
                ),
            })
        },
        |handler| handler.handle(&request.method, request.params.as_ref()),
    ) {
        Ok(result) => serde_json::json!({
            "type": "response",
            "id": request.id,
            "result": result,
        }),
        Err(error) => serde_json::json!({
            "type": "response",
            "id": request.id,
            "error": {
                "code": error.code,
                "message": error.message,
            },
        }),
    }
    .to_string()
}

/// The host's message asking a hook to stop.
pub(crate) const CANCEL_LINE: &str = r#"{"type":"cancel","reason":"timeout"}"#;

/// Read one line, refusing to grow past `max` bytes.
///
/// A line that exceeds the cap is an error rather than a truncated message: a
/// protocol message cut in half is not a smaller message, it is a different
/// one. The caller kills the hook when this returns an error.
pub(crate) async fn read_line_capped<R>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(line))
                };
            }

            match available.iter().position(|byte| *byte == b'\n') {
                Some(newline) => {
                    let take = newline + 1;
                    if line.len() + take > max {
                        return Err(too_long(max));
                    }
                    line.extend_from_slice(&available[..take]);
                    (take, true)
                }
                None => {
                    let take = available.len();
                    if line.len() + take > max {
                        return Err(too_long(max));
                    }
                    line.extend_from_slice(available);
                    (take, false)
                }
            }
        };

        reader.consume(consumed);
        if complete {
            return Ok(Some(line));
        }
    }
}

/// The error a capped read returns.
fn too_long(max: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("hook output line exceeds {max} bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_without_a_control_type_is_the_result() {
        let message = parse_message(r#"{"action":"continue"}"#).unwrap().unwrap();
        assert!(matches!(message, HookMessage::Result));
    }

    #[test]
    fn an_explicit_result_type_is_still_the_result() {
        let message = parse_message(r#"{"type":"result","action":"stop"}"#)
            .unwrap()
            .unwrap();
        assert!(matches!(message, HookMessage::Result));
    }

    #[test]
    fn an_event_carries_its_name_and_data() {
        let message = parse_message(r#"{"type":"event","event":"step","data":{"n":1}}"#)
            .unwrap()
            .unwrap();
        match message {
            HookMessage::Event(event) => {
                assert_eq!(event.event, "step");
                assert_eq!(event.data.unwrap()["n"], 1);
            }
            _ => panic!("not an event"),
        }
    }

    #[test]
    fn a_request_carries_id_method_and_params() {
        let message = parse_message(r#"{"type":"request","id":7,"method":"context","params":{}}"#)
            .unwrap()
            .unwrap();
        match message {
            HookMessage::Request(request) => {
                assert_eq!(request.id, 7);
                assert_eq!(request.method, "context");
                assert!(request.params.is_some());
            }
            _ => panic!("not a request"),
        }
    }

    #[test]
    fn a_log_line_is_not_an_answer() {
        let message = parse_message(r#"{"type":"log","message":"working"}"#)
            .unwrap()
            .unwrap();
        assert!(matches!(message, HookMessage::Log(_)));
    }

    #[test]
    fn blank_lines_are_ignored() {
        assert!(parse_message("  \n").unwrap().is_none());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_skipped_line() {
        assert!(parse_message("not json").is_err());
    }

    #[test]
    fn a_control_message_missing_its_field_is_an_error() {
        assert!(parse_message(r#"{"type":"event"}"#).is_err());
        assert!(parse_message(r#"{"type":"request","method":"x"}"#).is_err());
        assert!(parse_message(r#"{"type":"log"}"#).is_err());
    }

    #[test]
    fn a_response_with_no_handler_is_a_refusal() {
        let request = HookHostRequest {
            id: 5,
            method: "context".into(),
            params: None,
        };
        let line = response_line(&request, None);
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "response");
        assert_eq!(value["id"], 5);
        assert_eq!(value["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn a_response_with_a_handler_carries_its_result() {
        #[derive(Debug)]
        struct Echo;

        impl HookRequestHandler for Echo {
            fn handle(
                &self,
                _method: &str,
                _params: Option<&Value>,
            ) -> Result<Value, HookRequestError> {
                Ok(serde_json::json!({"ok": true}))
            }
        }

        let request = HookHostRequest {
            id: 9,
            method: "context".into(),
            params: None,
        };
        let line = response_line(&request, Some(&Echo));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["ok"], true);
    }

    #[tokio::test]
    async fn a_line_is_returned_only_once_it_is_complete() {
        let input: &[u8] = b"first\nsecond\n";
        let mut reader = tokio::io::BufReader::new(input);
        assert_eq!(
            read_line_capped(&mut reader, 100).await.unwrap().unwrap(),
            b"first\n"
        );
        assert_eq!(
            read_line_capped(&mut reader, 100).await.unwrap().unwrap(),
            b"second\n"
        );
        assert!(read_line_capped(&mut reader, 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_line_over_the_cap_is_an_error() {
        let input: &[u8] = b"0123456789\n";
        let mut reader = tokio::io::BufReader::new(input);
        let error = read_line_capped(&mut reader, 4).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn eof_without_a_newline_still_returns_the_last_line() {
        let input: &[u8] = b"tail";
        let mut reader = tokio::io::BufReader::new(input);
        assert_eq!(
            read_line_capped(&mut reader, 100).await.unwrap().unwrap(),
            b"tail"
        );
        assert!(read_line_capped(&mut reader, 100).await.unwrap().is_none());
    }
}
