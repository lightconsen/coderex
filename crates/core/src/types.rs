//! Public type definitions for CodeRex.

use serde::{Deserialize, Serialize};

/// Role of a message in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Role {
    /// System prompt or context.
    System,
    /// User input.
    User,
    /// Assistant response.
    Assistant,
    /// Tool execution result.
    Tool,
}

/// Content of a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MessageContent {
    /// Plain text content.
    Text(String),
    /// A tool call request.
    ToolCall(ToolCall),
    /// A tool execution result.
    ToolResult(ToolResult),
}

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// The role of the message sender.
    pub role: Role,
    /// The content of the message.
    pub content: MessageContent,
}

impl Message {
    /// Create a new system message.
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: MessageContent::Text(text.into()),
        }
    }

    /// Create a new user message.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: MessageContent::Text(text.into()),
        }
    }

    /// Create a new assistant message.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Text(text.into()),
        }
    }

    /// Create a new tool result message.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn tool(result: ToolResult) -> Self {
        Self {
            role: Role::Tool,
            content: MessageContent::ToolResult(result),
        }
    }
}

/// A request to invoke a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Name of the tool to invoke.
    pub name: String,
    /// JSON-encoded input arguments.
    pub input: serde_json::Value,
}

/// The result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// The tool call this result corresponds to.
    pub tool_call_id: String,
    /// Output from the tool.
    pub output: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
}

/// Token usage summary for an API response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummary {
    /// Number of input tokens.
    pub input_tokens: usize,
    /// Number of output tokens.
    pub output_tokens: usize,
    /// Number of tokens served from cache.
    pub cache_read_tokens: usize,
    /// Number of tokens written to cache.
    pub cache_creation_tokens: usize,
}

/// Identifier for an Anthropic model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(pub String);

impl ModelId {
    /// Claude Opus 4.6
    #[must_use]
    pub fn opus() -> Self {
        Self("claude-opus-4-6".into())
    }

    /// Claude Sonnet 4.6
    #[must_use]
    pub fn sonnet() -> Self {
        Self("claude-sonnet-4-6".into())
    }

    /// Claude Haiku 4.5
    #[must_use]
    pub fn haiku() -> Self {
        Self("claude-haiku-4-5-20251001".into())
    }
}

/// Events emitted by the engine to the UI/SDK.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineEvent {
    /// A text delta from the assistant.
    TextDelta(String),
    /// A tool call has started executing.
    ToolCallStart {
        /// Unique call identifier.
        id: String,
        /// Tool name.
        name: String,
    },
    /// A tool call has completed.
    ToolCallEnd {
        /// Unique call identifier.
        id: String,
        /// Execution result.
        result: ToolResult,
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
    /// An elevation request needs user input (elevated privileges required).
    ElevationRequest {
        /// Unique request identifier.
        id: String,
        /// Tool name requiring elevation.
        tool: String,
        /// Reason for elevation.
        reason: String,
    },
    /// The session has ended.
    SessionEnd,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── serde roundtrip tests (Phase 0.2 requirement) ──────────────────────

    #[test]
    fn role_roundtrip() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).unwrap();
            let decoded: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, role);
        }
    }

    #[test]
    fn message_text_roundtrip() {
        let msg = Message::user("hello world");
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.role, Role::User);
        assert!(matches!(decoded.content, MessageContent::Text(s) if s == "hello world"));
    }

    #[test]
    fn message_tool_result_roundtrip() {
        let result = ToolResult {
            tool_call_id: "tc_1".into(),
            output: "file content".into(),
            is_error: false,
        };
        let msg = Message::tool(result.clone());
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.role, Role::Tool);
        if let MessageContent::ToolResult(r) = &decoded.content {
            assert_eq!(r.tool_call_id, "tc_1");
            assert_eq!(r.output, "file content");
            assert!(!r.is_error);
        } else {
            panic!("expected ToolResult content");
        }
    }

    #[test]
    fn tool_call_roundtrip() {
        let tc = ToolCall {
            id: "tc_abc".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "Cargo.toml"}),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let decoded: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "tc_abc");
        assert_eq!(decoded.name, "Read");
        assert_eq!(decoded.input["path"], "Cargo.toml");
    }

    #[test]
    fn tool_result_roundtrip() {
        let tr = ToolResult {
            tool_call_id: "tc_xyz".into(),
            output: "success".into(),
            is_error: true,
        };
        let json = serde_json::to_string(&tr).unwrap();
        let decoded: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.tool_call_id, "tc_xyz");
        assert_eq!(decoded.output, "success");
        assert!(decoded.is_error);
    }

    #[test]
    fn usage_summary_roundtrip() {
        let usage = UsageSummary {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_creation_tokens: 100,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let decoded: UsageSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, usage);
    }

    #[test]
    fn model_id_roundtrip() {
        let model = ModelId::sonnet();
        let json = serde_json::to_string(&model).unwrap();
        let decoded: ModelId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, model);
        assert_eq!(decoded.0, "claude-sonnet-4-6");
    }

    #[test]
    fn message_content_text_serialization() {
        let content = MessageContent::Text("hi".into());
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("Text"));
    }

    #[test]
    fn message_content_tool_call_serialization() {
        let tc = ToolCall {
            id: "1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        let content = MessageContent::ToolCall(tc);
        let json = serde_json::to_string(&content).unwrap();
        let decoded: MessageContent = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, MessageContent::ToolCall(_)));
    }
}
