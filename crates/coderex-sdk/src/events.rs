//! SDK structured event stream.

use coderex_core::types::{EngineEvent, ToolCall, ToolResult};

/// Events emitted by the SDK.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SdkEvent {
    /// A text delta from the assistant.
    TextDelta(String),
    /// A tool call started.
    ToolCallStart {
        /// The tool call that was initiated.
        tool_call: ToolCall,
    },
    /// A tool call ended.
    ToolCallEnd {
        /// The result of the tool call.
        result: ToolResult,
    },
    /// A compact boundary was inserted.
    CompactBoundary,
    /// A permission request.
    PermissionRequest {
        /// The unique request identifier.
        id: String,
        /// The tool name requesting permission.
        tool: String,
        /// The arguments passed to the tool.
        args: serde_json::Value,
    },
    /// An elicitation request.
    ElicitationRequest {
        /// The unique request identifier.
        id: String,
        /// The elicitation message.
        message: String,
    },
    /// Session ended.
    SessionEnd,
    /// Engine event from the core.
    Engine(EngineEvent),
}
