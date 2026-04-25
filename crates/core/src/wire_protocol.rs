//! Wire protocol — typed event stream for decoupling engine from UI.
//!
//! The wire protocol enables headless/server/IDE integration by serializing
//! engine events as NDJSON (newline-delimited JSON) over any transport.
//!
//! ## Protocol
//!
//! Each line is a self-contained JSON object representing either a
//! `WireEvent` (server → client) or `WireRequest` (client → server).
//!
//! ```ndjson
//! {"type":"text_delta","delta":"Hello"}
//! {"type":"tool_call_start","id":"tc_1","name":"Read"}
//! {"type":"tool_call_end","id":"tc_1","result":{"tool_call_id":"tc_1","output":"...","is_error":false}}
//! {"type":"session_end"}
//! ```

use crate::types::{ToolResult, UsageSummary};
use serde::{Deserialize, Serialize};

/// Events emitted by the engine over the wire.
///
/// This is a serializable mirror of [`EngineEvent`](crate::types::EngineEvent)
/// with stable field names for external consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireEvent {
    /// A text delta from the assistant.
    TextDelta {
        /// The text chunk.
        delta: String,
    },
    /// A tool call has started executing.
    ToolCallStart {
        /// Unique call identifier.
        id: String,
        /// Tool name.
        name: String,
        /// Safe activity summary for interactive UI surfaces.
        summary: String,
    },
    /// A tool call has completed.
    ToolCallEnd {
        /// Unique call identifier.
        id: String,
        /// Execution result.
        result: WireToolResult,
    },
    /// A compact boundary marker was inserted.
    CompactBoundary,
    /// A permission request needs user input.
    PermissionRequest {
        /// Unique request identifier.
        id: String,
        /// Tool name.
        tool: String,
        /// Tool arguments.
        args: serde_json::Value,
    },
    /// An elicitation request needs user input.
    ElicitationRequest {
        /// Unique request identifier.
        id: String,
        /// Prompt message.
        message: String,
    },
    /// An elevation request needs user input.
    ElevationRequest {
        /// Unique request identifier.
        id: String,
        /// Tool name requiring elevation.
        tool: String,
        /// Reason for elevation.
        reason: String,
    },
    /// Token usage summary for the turn.
    UsageSummary {
        /// Input tokens consumed.
        input_tokens: usize,
        /// Output tokens produced.
        output_tokens: usize,
        /// Cache read tokens.
        cache_read_tokens: usize,
        /// Cache creation tokens.
        cache_creation_tokens: usize,
    },
    /// The turn has started.
    TurnStart {
        /// Turn number (1-indexed).
        turn_number: u32,
    },
    /// The turn has ended.
    TurnEnd {
        /// Turn number (1-indexed).
        turn_number: u32,
        /// Why the turn ended.
        stop_reason: String,
    },
    /// The session has ended.
    SessionEnd,
    /// A protocol-level error occurred.
    Error {
        /// Error message.
        message: String,
    },
    /// An event from a sub-agent.
    AgentEvent {
        /// Agent identifier.
        agent_id: String,
        /// The lifecycle event.
        event: crate::types::AgentLifecycleEvent,
    },
    /// A notification message.
    Notification {
        /// Notification text.
        message: String,
    },
    /// A permission explanation was generated for a tool request.
    PermissionExplanation {
        /// Tool name.
        tool: String,
        /// Generated explanation text.
        explanation: String,
    },
}

/// Requests sent by the client over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireRequest {
    /// Submit a user prompt to the engine.
    Submit {
        /// The user's message.
        prompt: String,
        /// Optional system prompt override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        /// Maximum turns to allow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_turns: Option<u32>,
    },
    /// Respond to a permission request.
    PermissionResponse {
        /// The request ID being responded to.
        id: String,
        /// Whether permission was granted.
        granted: bool,
    },
    /// Respond to an elicitation request.
    ElicitationResponse {
        /// The request ID being responded to.
        id: String,
        /// The user's response.
        response: String,
    },
    /// Respond to an elevation request.
    ElevationResponse {
        /// The request ID being responded to.
        id: String,
        /// Whether elevation was granted.
        granted: bool,
    },
    /// Interrupt the current turn.
    Interrupt,
}

/// Serializable tool result for the wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireToolResult {
    /// The tool call this result corresponds to.
    pub tool_call_id: String,
    /// Output from the tool.
    pub output: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
}

impl From<ToolResult> for WireToolResult {
    fn from(r: ToolResult) -> Self {
        Self {
            tool_call_id: r.tool_call_id,
            output: r.output,
            is_error: r.is_error,
        }
    }
}

impl From<WireToolResult> for ToolResult {
    fn from(r: WireToolResult) -> Self {
        Self {
            tool_call_id: r.tool_call_id,
            output: r.output,
            is_error: r.is_error,
        }
    }
}

impl From<UsageSummary> for WireEvent {
    fn from(u: UsageSummary) -> Self {
        Self::UsageSummary {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
        }
    }
}

/// Encode a wire event as an NDJSON line.
#[must_use]
pub fn encode_event(event: &WireEvent) -> String {
    match serde_json::to_string(event) {
        Ok(json) => format!("{json}\n"),
        Err(e) => format!(
            "{{\"type\":\"error\",\"message\":{}}}\n",
            serde_json::json!(e.to_string())
        ),
    }
}

/// Encode a wire request as an NDJSON line.
#[must_use]
pub fn encode_request(req: &WireRequest) -> String {
    match serde_json::to_string(req) {
        Ok(json) => format!("{json}\n"),
        Err(e) => format!(
            "{{\"type\":\"error\",\"message\":{}}}\n",
            serde_json::json!(e.to_string())
        ),
    }
}

/// Decode an NDJSON line into a `WireEvent`.
///
/// Returns `None` for empty lines.
pub fn decode_event(line: &str) -> Option<Result<WireEvent, serde_json::Error>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(serde_json::from_str(trimmed))
}

/// Decode an NDJSON line into a `WireRequest`.
///
/// Returns `None` for empty lines.
pub fn decode_request(line: &str) -> Option<Result<WireRequest, serde_json::Error>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(serde_json::from_str(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_text_delta() {
        let event = WireEvent::TextDelta {
            delta: "hello".into(),
        };
        let json = encode_event(&event);
        let decoded = decode_event(&json).unwrap().unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn roundtrip_tool_call_start() {
        let event = WireEvent::ToolCallStart {
            id: "tc_1".into(),
            name: "Read".into(),
            summary: "Reading src/lib.rs...".into(),
        };
        let json = encode_event(&event);
        let decoded = decode_event(&json).unwrap().unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn roundtrip_submit_request() {
        let req = WireRequest::Submit {
            prompt: "hello".into(),
            system_prompt: None,
            max_turns: Some(10),
        };
        let json = encode_request(&req);
        let decoded = decode_request(&json).unwrap().unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn decode_ignores_empty_lines() {
        assert!(decode_event("").is_none());
        assert!(decode_event("   ").is_none());
    }

    #[test]
    fn event_has_snake_case_tag() {
        let json = encode_event(&WireEvent::CompactBoundary);
        assert!(json.contains("\"type\":\"compact_boundary\""));
    }
}
