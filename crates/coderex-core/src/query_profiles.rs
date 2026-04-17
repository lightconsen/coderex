//! Query profiles — model, prompt, and request abstractions.
//!
//! This module defines the profiles and request types used by the orchestrator
//! layer to route AI calls by intent rather than raw parameters.
//!
//! Three profile types decouple business logic from concrete model/prompt names:
//! - `ModelProfile`: selects which model to use (main, lightweight, background)
//! - `PromptProfile`: selects which system prompt to use (default, advisor, etc.)
//! - `QueryKind`: selects which call pattern to use (main, side, lightweight, forked)

use crate::types::ModelId;

// ─── Model Profile ──────────────────────────────────────────────────────────

/// Selects which model to use without hardcoding model IDs in business logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelProfile {
    /// The main model for interactive coding sessions.
    #[default]
    Main,
    /// A fast, small model for lightweight tasks (titles, parsing, classification).
    Lightweight,
    /// A model for background/forked agent tasks.
    Background,
}

impl ModelProfile {
    /// Resolve the model profile to a concrete ModelId.
    #[must_use]
    pub fn resolve(&self) -> ModelId {
        match self {
            Self::Main => ModelId::sonnet(),
            Self::Lightweight => ModelId::haiku(),
            Self::Background => ModelId::sonnet(),
        }
    }
}

// ─── Prompt Profile ─────────────────────────────────────────────────────────

/// Selects which system prompt profile to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum PromptProfile {
    /// Default agent system prompt (general coding).
    #[default]
    DefaultAgent,
    /// Advisor mode — guidance-focused, less implementation.
    Advisor,
    /// Bug hunter mode — systematic code review for defects.
    BugHunter,
    /// Reviewer mode — code review and analysis.
    Reviewer,
    /// Security reviewer mode — security-focused diff and code review.
    SecurityReviewer,
    /// Memory updater — focused on session memory maintenance.
    MemoryUpdater,
    /// Title generator — short session title from conversation.
    TitleGenerator,
    /// Permission explainer — explains tool call risks to the user.
    PermissionExplainer,
}

// ─── Query Kind ─────────────────────────────────────────────────────────────

/// Selects which call pattern / execution strategy to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// Full main agent loop with tools, streaming, and multi-turn execution.
    MainAgent,
    /// Single-shot non-streaming query for one-off decisions or explanations.
    SideQuery,
    /// Lightweight single-shot query using a small model for fast responses.
    Lightweight,
    /// Forked sub-agent with isolated context and optional tool access.
    ForkedAgent,
}

// ─── Query Request ──────────────────────────────────────────────────────────

/// High-level request specification that the orchestrator translates into
/// concrete API call parameters.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    /// Which call pattern to use.
    pub kind: QueryKind,
    /// The user's message or task description.
    pub user_prompt: String,
    /// Optional system prompt that fully overrides the default.
    pub custom_system_prompt: Option<String>,
    /// Optional system prompt appended after the base system prompt.
    pub append_system_prompt: Option<String>,
    /// Prompt profile for selecting the system prompt template.
    pub prompt_profile: PromptProfile,
    /// Allowed tool names (None = use default set for the profile).
    pub allowed_tools: Option<Vec<String>>,
    /// Which model to use (profile → resolved model).
    pub model_profile: ModelProfile,
    /// Whether to inherit the current session's message history.
    pub inherit_session: bool,
    /// Whether to stream the response.
    pub stream: bool,
    /// Maximum tokens for the response.
    pub max_tokens: u32,
}

impl QueryRequest {
    /// Create a lightweight side query with a simple prompt.
    #[must_use]
    pub fn side_query(user_prompt: impl Into<String>) -> Self {
        Self {
            kind: QueryKind::SideQuery,
            user_prompt: user_prompt.into(),
            custom_system_prompt: None,
            append_system_prompt: None,
            prompt_profile: PromptProfile::DefaultAgent,
            allowed_tools: None,
            model_profile: ModelProfile::Main,
            inherit_session: false,
            stream: false,
            max_tokens: 1024,
        }
    }

    /// Create a lightweight query using a small model.
    #[must_use]
    pub fn lightweight(user_prompt: impl Into<String>) -> Self {
        Self {
            kind: QueryKind::Lightweight,
            user_prompt: user_prompt.into(),
            custom_system_prompt: None,
            append_system_prompt: None,
            prompt_profile: PromptProfile::DefaultAgent,
            allowed_tools: None,
            model_profile: ModelProfile::Lightweight,
            inherit_session: false,
            stream: false,
            max_tokens: 512,
        }
    }

    /// Create a title generator request.
    #[must_use]
    pub fn title(conversation: impl Into<String>) -> Self {
        Self {
            kind: QueryKind::Lightweight,
            user_prompt: format!(
                "Generate a 3-7 word sentence-case title for this conversation. \
                 Return only JSON: {{\"title\": \"...\"}}\n\nConversation:\n{conv}",
                conv = conversation.into()
            ),
            custom_system_prompt: None,
            append_system_prompt: None,
            prompt_profile: PromptProfile::TitleGenerator,
            allowed_tools: None,
            model_profile: ModelProfile::Lightweight,
            inherit_session: false,
            stream: false,
            max_tokens: 128,
        }
    }

    /// Create a permission explanation request.
    #[must_use]
    pub fn permission_explain(tool_name: &str, tool_input: &str) -> Self {
        Self {
            kind: QueryKind::SideQuery,
            user_prompt: format!(
                "Explain the potential risks of this tool call in 1-2 sentences:\n\
                 Tool: {tool_name}\nInput: {tool_input}",
            ),
            custom_system_prompt: None,
            append_system_prompt: None,
            prompt_profile: PromptProfile::PermissionExplainer,
            allowed_tools: None,
            model_profile: ModelProfile::Main,
            inherit_session: false,
            stream: false,
            max_tokens: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_profile_resolves() {
        assert_eq!(ModelProfile::Main.resolve(), ModelId::sonnet());
        assert_eq!(ModelProfile::Lightweight.resolve(), ModelId::haiku());
        assert_eq!(ModelProfile::Background.resolve(), ModelId::sonnet());
    }

    #[test]
    fn model_profile_default_is_main() {
        assert_eq!(ModelProfile::default(), ModelProfile::Main);
    }

    #[test]
    fn prompt_profile_default() {
        assert_eq!(PromptProfile::default(), PromptProfile::DefaultAgent);
    }

    #[test]
    fn query_request_side_query_defaults() {
        let req = QueryRequest::side_query("test");
        assert_eq!(req.kind, QueryKind::SideQuery);
        assert_eq!(req.user_prompt, "test");
        assert_eq!(req.max_tokens, 1024);
        assert!(!req.stream);
        assert!(!req.inherit_session);
    }

    #[test]
    fn query_request_lightweight_defaults() {
        let req = QueryRequest::lightweight("classify this");
        assert_eq!(req.kind, QueryKind::Lightweight);
        assert_eq!(req.model_profile, ModelProfile::Lightweight);
        assert_eq!(req.max_tokens, 512);
    }

    #[test]
    fn query_request_title_builder() {
        let req = QueryRequest::title("user asked about Rust");
        assert_eq!(req.prompt_profile, PromptProfile::TitleGenerator);
        assert!(req.user_prompt.contains("3-7 word"));
        assert!(req.user_prompt.contains("Rust"));
    }

    #[test]
    fn query_request_permission_explain_builder() {
        let req = QueryRequest::permission_explain("Bash", "rm -rf /");
        assert_eq!(req.prompt_profile, PromptProfile::PermissionExplainer);
        assert!(req.user_prompt.contains("Bash"));
        assert!(req.user_prompt.contains("rm -rf /"));
    }

    #[test]
    fn query_request_full_custom() {
        let req = QueryRequest {
            kind: QueryKind::MainAgent,
            user_prompt: "hello".into(),
            custom_system_prompt: Some("custom".into()),
            append_system_prompt: Some("append".into()),
            prompt_profile: PromptProfile::Advisor,
            allowed_tools: Some(vec!["Read".into()]),
            model_profile: ModelProfile::Main,
            inherit_session: true,
            stream: true,
            max_tokens: 4096,
        };
        assert_eq!(req.custom_system_prompt.as_deref(), Some("custom"));
        assert_eq!(req.append_system_prompt.as_deref(), Some("append"));
        assert_eq!(req.allowed_tools.as_deref().unwrap(), &["Read"]);
    }
}
