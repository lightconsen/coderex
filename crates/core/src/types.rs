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

impl ToolCall {
    /// Build a short, safe activity summary for interactive UI surfaces.
    #[must_use]
    pub fn activity_summary(&self) -> String {
        Self::summarize_activity(&self.name, &self.input)
    }

    /// Build a short, safe activity summary for a tool name and input payload.
    #[must_use]
    pub fn summarize_activity(name: &str, input: &serde_json::Value) -> String {
        match name.to_ascii_lowercase().as_str() {
            "read" => extract_tool_string(input, &["file_path", "path", "file"])
                .map(|path| format!("Reading {}...", summarize_tool_text(path)))
                .unwrap_or_else(|| "Reading files...".into()),
            "grep" | "rg" => extract_tool_string(input, &["pattern"])
                .map(|pattern| format!("Searching for {}...", summarize_tool_text(pattern)))
                .unwrap_or_else(|| "Searching code...".into()),
            "glob" => extract_tool_string(input, &["pattern"])
                .map(|pattern| {
                    format!("Finding files matching {}...", summarize_tool_text(pattern))
                })
                .unwrap_or_else(|| "Discovering files...".into()),
            "currentdir" | "current_dir" => "Checking current directory...".into(),
            "listdir" | "list_dir" => extract_tool_string(input, &["path"])
                .map(|path| format!("Listing files in {}...", summarize_tool_text(path)))
                .unwrap_or_else(|| "Listing files in current directory...".into()),
            "bash" | "shell" => extract_tool_string(input, &["command", "cmd"])
                .map(|command| format!("Running {}...", summarize_tool_text(command)))
                .unwrap_or_else(|| "Running shell command...".into()),
            "write" => extract_tool_string(input, &["file_path", "path", "file"])
                .map(|path| format!("Writing {}...", summarize_tool_text(path)))
                .unwrap_or_else(|| "Writing file...".into()),
            "edit" => extract_tool_string(input, &["file_path", "path", "file"])
                .map(|path| format!("Updating {}...", summarize_tool_text(path)))
                .unwrap_or_else(|| "Applying changes...".into()),
            "apply_patch" => "Applying changes...".into(),
            "task" => "Delegating task...".into(),
            "web_fetch" => extract_tool_string(input, &["url"])
                .map(|url| format!("Fetching {}...", summarize_tool_text(url)))
                .unwrap_or_else(|| "Fetching from the web...".into()),
            "web_search" => extract_tool_string(input, &["query"])
                .map(|query| format!("Searching the web for {}...", summarize_tool_text(query)))
                .unwrap_or_else(|| "Searching the web...".into()),
            _ => format!("Running {name}..."),
        }
    }
}

fn extract_tool_string<'a>(input: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(serde_json::Value::as_str))
}

fn summarize_tool_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 60;
    if collapsed.chars().count() <= MAX_CHARS {
        collapsed
    } else {
        let clipped: String = collapsed.chars().take(MAX_CHARS - 3).collect();
        format!("{clipped}...")
    }
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

    /// Return the model's context window size in tokens.
    ///
    /// Compaction thresholds are derived from this value (50% trigger, 90%
    /// reactive), so over-reporting it means context overflows before anything
    /// compacts. Known Claude models are matched by family prefix so dated
    /// variants (`claude-sonnet-4-6-20260101`) resolve correctly; anything
    /// unrecognised — a gateway alias, a local model, a future release — gets
    /// a deliberately conservative floor rather than the largest window we
    /// happen to know, so the failure mode is early compaction rather than a
    /// hard context-length error.
    #[must_use]
    pub fn context_window(&self) -> usize {
        Self::context_window_for(&self.0)
    }

    /// Context window for a raw model name.
    #[must_use]
    pub fn context_window_for(name: &str) -> usize {
        // Claude families all ship a 200k window today.
        if name.starts_with("claude-") {
            return CLAUDE_CONTEXT_WINDOW;
        }
        UNKNOWN_CONTEXT_WINDOW
    }
}

/// Context window for Claude model families.
pub const CLAUDE_CONTEXT_WINDOW: usize = 200_000;

/// Conservative window assumed for models we cannot identify.
///
/// Chosen so that an unknown model compacts early instead of overflowing: a
/// wrong guess that is too small costs some context, a wrong guess that is too
/// large loses the conversation.
pub const UNKNOWN_CONTEXT_WINDOW: usize = 32_000;

/// Lifecycle events for a sub-agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentLifecycleEvent {
    /// Agent started execution.
    Started,
    /// Agent completed its task.
    Completed {
        /// Summary of work done.
        summary: String,
    },
    /// Agent failed.
    Failed {
        /// Error message.
        error: String,
    },
    /// Agent is idle.
    Idle {
        /// Reason for idleness.
        reason: String,
    },
    /// Agent executed a tool call.
    ToolCall {
        /// Tool name.
        name: String,
        /// Tool arguments.
        args: serde_json::Value,
    },
    /// Agent produced a text delta.
    TextDelta(String),
}

/// Events emitted by the engine to the UI/SDK.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        /// Safe activity summary for interactive UI surfaces.
        summary: String,
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
    /// An event from a sub-agent.
    AgentEvent {
        /// Agent identifier.
        agent_id: String,
        /// The lifecycle event.
        event: AgentLifecycleEvent,
    },
    /// A notification message (e.g., API degradation warning).
    Notification {
        /// Notification message.
        message: String,
    },
    /// A permission explanation was generated for a tool request.
    PermissionExplanation {
        /// Tool name.
        tool: String,
        /// Generated explanation text.
        explanation: String,
    },
    /// The streaming response was interrupted mid-turn.
    StreamInterrupted {
        /// "error" (failed; partial content may be missing) or the provider
        /// stop reason such as "max_tokens" (legitimate truncation).
        reason: String,
        /// Length of partial text already delivered to the UI.
        partial_len: usize,
        /// Whether the engine will automatically re-request.
        will_retry: bool,
    },
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

    #[test]
    fn tool_call_activity_summary_uses_safe_specific_labels() {
        assert_eq!(
            ToolCall::summarize_activity("Read", &serde_json::json!({"file_path":"src/lib.rs"})),
            "Reading src/lib.rs..."
        );
        assert_eq!(
            ToolCall::summarize_activity(
                "Bash",
                &serde_json::json!({"command":"cargo test -p coderex-cli"})
            ),
            "Running cargo test -p coderex-cli..."
        );
        assert_eq!(
            ToolCall::summarize_activity("Write", &serde_json::json!({"file_path":"src/lib.rs"})),
            "Writing src/lib.rs..."
        );
        assert_eq!(
            ToolCall::summarize_activity("CurrentDir", &serde_json::json!({})),
            "Checking current directory..."
        );
        assert_eq!(
            ToolCall::summarize_activity("ListDir", &serde_json::json!({})),
            "Listing files in current directory..."
        );
    }

    #[test]
    fn tool_call_activity_summary_truncates_long_inputs() {
        let summary = ToolCall::summarize_activity(
            "Bash",
            &serde_json::json!({"command":"cargo test -p coderex-cli --test very_long_case_name -- --ignored --nocapture --exact"}),
        );
        assert!(summary.starts_with("Running cargo test -p coderex-cli"));
        assert!(summary.ends_with("..."));
        assert!(summary.chars().count() <= 71);
    }
}

#[cfg(test)]
mod context_window_tests {
    use super::*;

    #[test]
    fn known_families_report_their_full_window() {
        assert_eq!(ModelId::opus().context_window(), CLAUDE_CONTEXT_WINDOW);
        assert_eq!(ModelId::sonnet().context_window(), CLAUDE_CONTEXT_WINDOW);
        assert_eq!(ModelId::haiku().context_window(), CLAUDE_CONTEXT_WINDOW);
    }

    /// Dated releases and aliases must resolve by family, not by exact string.
    #[test]
    fn family_prefix_covers_dated_variants() {
        for name in [
            "claude-sonnet-4-6-20260101",
            "claude-opus-4-6-20260215",
            "claude-3-5-haiku-20241022",
        ] {
            assert_eq!(
                ModelId(name.to_string()).context_window(),
                CLAUDE_CONTEXT_WINDOW,
                "{name} should resolve to the Claude window"
            );
        }
    }

    /// The bug this guards: an unknown model previously reported 200k, so
    /// compaction would not trigger until ~180k tokens.
    #[test]
    fn unknown_models_are_conservative_not_maximal() {
        for name in ["gpt-4o", "local-llama-3", "my-gateway-alias"] {
            let window = ModelId(name.to_string()).context_window();
            assert_eq!(window, UNKNOWN_CONTEXT_WINDOW, "{name}");
            assert!(
                window < CLAUDE_CONTEXT_WINDOW,
                "unknown models must not claim the largest known window"
            );
        }
    }

    #[test]
    fn case_sensitivity_is_a_known_limitation() {
        // Model ids are lowercase in practice; document the boundary rather
        // than silently normalising, so a mismatch shows up in the window.
        assert_eq!(
            ModelId("CLAUDE-SONNET-4-6".to_string()).context_window(),
            UNKNOWN_CONTEXT_WINDOW
        );
    }
}
