//! Wire adapter — bridge between `EngineEvent` and `WireEvent`.
//!
//! Provides bidirectional conversion between the engine's internal
//! [`EngineEvent`] type and the serializable [`WireEvent`] wire protocol
//! type, plus utilities for bridging an mpsc channel to a [`WireTransport`].

use crate::types::{EngineEvent, ToolResult};
use crate::wire_protocol::{WireEvent, WireToolResult};
use crate::wire_transport::WireTransport;
use tokio::sync::mpsc;

/// Convert an [`EngineEvent`] to a [`WireEvent`].
#[must_use]
pub fn engine_event_to_wire(event: &EngineEvent) -> Option<WireEvent> {
    Some(match event {
        EngineEvent::TextDelta(delta) => WireEvent::TextDelta {
            delta: delta.clone(),
        },
        EngineEvent::ToolCallStart { id, name, summary } => WireEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
            summary: summary.clone(),
        },
        EngineEvent::ToolCallEnd { id, result } => WireEvent::ToolCallEnd {
            id: id.clone(),
            result: WireToolResult::from(result.clone()),
        },
        EngineEvent::CompactBoundary => WireEvent::CompactBoundary,
        EngineEvent::PermissionRequest { id, tool, args } => WireEvent::PermissionRequest {
            id: id.clone(),
            tool: tool.clone(),
            args: args.clone(),
        },
        EngineEvent::ElicitationRequest { id, message } => WireEvent::ElicitationRequest {
            id: id.clone(),
            message: message.clone(),
        },
        EngineEvent::ElevationRequest { id, tool, reason } => WireEvent::ElevationRequest {
            id: id.clone(),
            tool: tool.clone(),
            reason: reason.clone(),
        },
        EngineEvent::SessionEnd => WireEvent::SessionEnd,
        EngineEvent::AgentEvent { agent_id, event } => WireEvent::AgentEvent {
            agent_id: agent_id.clone(),
            event: event.clone(),
        },
        EngineEvent::Notification { message } => WireEvent::Notification {
            message: message.clone(),
        },
        EngineEvent::PermissionExplanation { tool, explanation } => {
            WireEvent::PermissionExplanation {
                tool: tool.clone(),
                explanation: explanation.clone(),
            }
        }
        EngineEvent::StreamInterrupted {
            reason,
            partial_len,
            will_retry,
        } => WireEvent::StreamInterrupted {
            reason: reason.clone(),
            partial_len: *partial_len,
            will_retry: *will_retry,
        },
    })
}

/// Convert a [`WireEvent`] back to an [`EngineEvent`].
///
/// Returns `None` for wire-only events like `UsageSummary` or `TurnStart`
/// that have no direct engine equivalent.
#[must_use]
pub fn wire_event_to_engine(event: &WireEvent) -> Option<EngineEvent> {
    Some(match event {
        WireEvent::TextDelta { delta } => EngineEvent::TextDelta(delta.clone()),
        WireEvent::ToolCallStart { id, name, summary } => EngineEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
            summary: summary.clone(),
        },
        WireEvent::ToolCallEnd { id, result } => EngineEvent::ToolCallEnd {
            id: id.clone(),
            result: ToolResult::from(result.clone()),
        },
        WireEvent::CompactBoundary => EngineEvent::CompactBoundary,
        WireEvent::PermissionRequest { id, tool, args } => EngineEvent::PermissionRequest {
            id: id.clone(),
            tool: tool.clone(),
            args: args.clone(),
        },
        WireEvent::ElicitationRequest { id, message } => EngineEvent::ElicitationRequest {
            id: id.clone(),
            message: message.clone(),
        },
        WireEvent::ElevationRequest { id, tool, reason } => EngineEvent::ElevationRequest {
            id: id.clone(),
            tool: tool.clone(),
            reason: reason.clone(),
        },
        WireEvent::SessionEnd => EngineEvent::SessionEnd,
        WireEvent::AgentEvent { agent_id, event } => EngineEvent::AgentEvent {
            agent_id: agent_id.clone(),
            event: event.clone(),
        },
        WireEvent::Notification { message } => EngineEvent::Notification {
            message: message.clone(),
        },
        WireEvent::PermissionExplanation { tool, explanation } => {
            EngineEvent::PermissionExplanation {
                tool: tool.clone(),
                explanation: explanation.clone(),
            }
        }
        WireEvent::StreamInterrupted {
            reason,
            partial_len,
            will_retry,
        } => EngineEvent::StreamInterrupted {
            reason: reason.clone(),
            partial_len: *partial_len,
            will_retry: *will_retry,
        },
        WireEvent::UsageSummary { .. }
        | WireEvent::TurnStart { .. }
        | WireEvent::TurnEnd { .. }
        | WireEvent::Error { .. } => return None,
    })
}

/// Bridge an mpsc receiver of [`EngineEvent`] to a [`WireTransport`].
///
/// Runs until the receiver is closed or the transport errors.
///
/// # Errors
/// Returns an I/O error if the transport fails to send.
pub async fn bridge_engine_to_wire<T: WireTransport>(
    mut event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    mut transport: T,
) -> std::io::Result<()> {
    while let Some(event) = event_rx.recv().await {
        if let Some(wire_event) = engine_event_to_wire(&event) {
            transport.send_event(wire_event).await?;
        }
    }
    transport.close().await
}

/// A wrapper around [`mpsc::UnboundedSender<EngineEvent>`] that also
/// forwards to a [`WireTransport`].
///
/// This allows the [`QueryEngine`] to emit to both the existing
/// mpsc channel (for the REPL UI) and the wire transport (for
/// headless consumers) simultaneously.
#[allow(missing_debug_implementations)]
pub struct DualEventSink<T: WireTransport> {
    tx: mpsc::UnboundedSender<EngineEvent>,
    transport: T,
}

impl<T: WireTransport> DualEventSink<T> {
    /// Create a new dual sink.
    #[must_use]
    pub const fn new(tx: mpsc::UnboundedSender<EngineEvent>, transport: T) -> Self {
        Self { tx, transport }
    }

    /// Send an event to both the channel and the wire transport.
    ///
    /// # Errors
    /// Returns an error if the wire transport fails. The mpsc channel
    /// send is best-effort (errors are silently dropped).
    pub async fn send(&mut self, event: EngineEvent) -> std::io::Result<()> {
        let _ = self.tx.send(event.clone());
        if let Some(wire_event) = engine_event_to_wire(&event) {
            self.transport.send_event(wire_event).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolResult;

    #[test]
    fn text_delta_roundtrip() {
        let engine = EngineEvent::TextDelta("hello".into());
        let wire = engine_event_to_wire(&engine).unwrap();
        let back = wire_event_to_engine(&wire).unwrap();
        assert!(matches!(back, EngineEvent::TextDelta(ref s) if s == "hello"));
    }

    #[test]
    fn tool_call_end_roundtrip() {
        let engine = EngineEvent::ToolCallEnd {
            id: "tc_1".into(),
            result: ToolResult {
                tool_call_id: "tc_1".into(),
                output: "file content".into(),
                is_error: false,
                images: Vec::new(),
            },
        };
        let wire = engine_event_to_wire(&engine).unwrap();
        let back = wire_event_to_engine(&wire).unwrap();
        assert!(matches!(back, EngineEvent::ToolCallEnd { id, .. } if id == "tc_1"));
    }

    #[test]
    fn session_end_roundtrip() {
        let engine = EngineEvent::SessionEnd;
        let wire = engine_event_to_wire(&engine).unwrap();
        let back = wire_event_to_engine(&wire).unwrap();
        assert!(matches!(back, EngineEvent::SessionEnd));
    }
}
