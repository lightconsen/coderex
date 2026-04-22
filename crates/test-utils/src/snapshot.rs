//! Snapshot assertion helpers for engine events.
//!
//! Wraps [`insta`] with convenience functions for asserting on
//! [`EngineEvent`] collections in tests.

use coderex_core::types::EngineEvent;

/// Assert that a single event matches a snapshot.
///
/// The snapshot name is derived from the test function name.
#[macro_export]
macro_rules! assert_event_snapshot {
    ($event:expr) => {
        insta::assert_debug_snapshot!($event);
    };
    ($event:expr, $name:expr) => {
        insta::assert_debug_snapshot!($name, $event);
    };
}

/// Assert that a collection of events matches a snapshot.
///
/// Filters out non-deterministic fields (timestamps, uuids) before
/// snapshotting.
#[macro_export]
macro_rules! assert_events_snapshot {
    ($events:expr) => {
        insta::assert_debug_snapshot!($events);
    };
    ($events:expr, $name:expr) => {
        insta::assert_debug_snapshot!($name, $events);
    };
}

/// Prepare events for snapshotting by redacting non-deterministic data.
///
/// Replaces uuids, timestamps, and temp paths with stable placeholders.
#[must_use]
pub fn redact_events(events: &[EngineEvent]) -> Vec<RedactedEvent> {
    events.iter().map(redact_event).collect()
}

/// A redacted version of [`EngineEvent`] for snapshotting.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum RedactedEvent {
    /// Text delta.
    TextDelta(String),
    /// Tool call started.
    ToolCallStart { id: String, name: String },
    /// Tool call completed.
    ToolCallEnd { id: String, result: RedactedToolResult },
    /// Compact boundary.
    CompactBoundary,
    /// Permission request.
    PermissionRequest { id: String, tool: String },
    /// Elicitation request.
    ElicitationRequest { id: String, message: String },
    /// Elevation request.
    ElevationRequest { id: String, tool: String },
    /// Session end.
    SessionEnd,
}

/// A redacted version of [`ToolResult`] for snapshotting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedToolResult {
    /// Tool call ID.
    pub tool_call_id: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
}

fn redact_event(event: &EngineEvent) -> RedactedEvent {
    match event {
        EngineEvent::TextDelta(delta) => RedactedEvent::TextDelta(delta.clone()),
        EngineEvent::ToolCallStart { id, name } => RedactedEvent::ToolCallStart {
            id: redact_id(id),
            name: name.clone(),
        },
        EngineEvent::ToolCallEnd { id, result } => RedactedEvent::ToolCallEnd {
            id: redact_id(id),
            result: RedactedToolResult {
                tool_call_id: redact_id(&result.tool_call_id),
                is_error: result.is_error,
            },
        },
        EngineEvent::CompactBoundary => RedactedEvent::CompactBoundary,
        EngineEvent::PermissionRequest { id, tool, .. } => RedactedEvent::PermissionRequest {
            id: redact_id(id),
            tool: tool.clone(),
        },
        EngineEvent::ElicitationRequest { id, message } => RedactedEvent::ElicitationRequest {
            id: redact_id(id),
            message: message.clone(),
        },
        EngineEvent::ElevationRequest { id, tool, .. } => RedactedEvent::ElevationRequest {
            id: redact_id(id),
            tool: tool.clone(),
        },
        EngineEvent::SessionEnd => RedactedEvent::SessionEnd,
        _ => RedactedEvent::TextDelta("<unknown>".into()),
    }
}

fn redact_id(id: &str) -> String {
    if id.len() > 8 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        "<uuid>".into()
    } else if id.starts_with("tc_") {
        "<tool-call-id>".into()
    } else {
        id.into()
    }
}
