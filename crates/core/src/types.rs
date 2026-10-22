//! Public type definitions for CodeRex.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// A provider thinking block from an extended-thinking response.
///
/// Providers in thinking mode emit this block before the visible text and
/// tool_use blocks and require it to be sent back unchanged on the next
/// request. `signature` is an opaque provider token and must be round-tripped
/// verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThinkingBlock {
    /// The reasoning text.
    pub thinking: String,
    /// Opaque provider signature for the block.
    pub signature: String,
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
    /// A provider thinking block that must be round-tripped.
    Thinking(ThinkingBlock),
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
            "read" => extract_tool_string(input, &["file_path", "path", "file"]).map_or_else(
                || "Reading files...".into(),
                |path| format!("Reading {}...", summarize_tool_text(path)),
            ),
            "grep" | "rg" => extract_tool_string(input, &["pattern"]).map_or_else(
                || "Searching code...".into(),
                |pattern| format!("Searching for {}...", summarize_tool_text(pattern)),
            ),
            "glob" => extract_tool_string(input, &["pattern"]).map_or_else(
                || "Discovering files...".into(),
                |pattern| format!("Finding files matching {}...", summarize_tool_text(pattern)),
            ),
            "currentdir" | "current_dir" => "Checking current directory...".into(),
            "listdir" | "list_dir" => extract_tool_string(input, &["path"]).map_or_else(
                || "Listing files in current directory...".into(),
                |path| format!("Listing files in {}...", summarize_tool_text(path)),
            ),
            "bash" | "shell" => extract_tool_string(input, &["command", "cmd"]).map_or_else(
                || "Running shell command...".into(),
                |command| format!("Running {}...", summarize_tool_text(command)),
            ),
            "write" => extract_tool_string(input, &["file_path", "path", "file"]).map_or_else(
                || "Writing file...".into(),
                |path| format!("Writing {}...", summarize_tool_text(path)),
            ),
            "edit" => extract_tool_string(input, &["file_path", "path", "file"]).map_or_else(
                || "Applying changes...".into(),
                |path| format!("Updating {}...", summarize_tool_text(path)),
            ),
            "apply_patch" => "Applying changes...".into(),
            "task" => "Delegating task...".into(),
            "web_fetch" => extract_tool_string(input, &["url"]).map_or_else(
                || "Fetching from the web...".into(),
                |url| format!("Fetching {}...", summarize_tool_text(url)),
            ),
            "web_search" => extract_tool_string(input, &["query"]).map_or_else(
                || "Searching the web...".into(),
                |query| format!("Searching the web for {}...", summarize_tool_text(query)),
            ),
            _ => format!("Running {name}..."),
        }
    }
}

fn extract_tool_string<'a>(input: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(serde_json::Value::as_str))
}

fn summarize_tool_text(text: &str) -> String {
    const MAX_CHARS: usize = 60;

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_CHARS {
        collapsed
    } else {
        let clipped: String = collapsed.chars().take(MAX_CHARS - 3).collect();
        format!("{clipped}...")
    }
}

/// An inline image carried by a tool result.
///
/// `data` is base64-encoded image bytes without a data-URI prefix, matching
/// the shape providers expect for base64 image sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    /// MIME type of the image, e.g. `image/png`.
    pub media_type: String,
    /// Base64-encoded image bytes, without a data-URI prefix.
    pub data: String,
}

/// The result of a tool execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolResult {
    /// The tool call this result corresponds to.
    pub tool_call_id: String,
    /// Output from the tool.
    pub output: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
    /// Images returned by the tool, forwarded to the model as image blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageBlock>,
}

impl ToolResult {
    /// Build a text-only tool result.
    #[must_use]
    pub fn text(tool_call_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            output: output.into(),
            is_error: false,
            images: Vec::new(),
        }
    }
}

/// Token usage summary for an API response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// This keeps the original API shape but now consults the full override
    /// chain: environment, the client-data file, the built-in model table, and
    /// finally the conservative unknown floor. Settings and experiment
    /// overrides are supplied by the config crate through
    /// [`ModelId::context_window_with`] and [`ContextWindowResolver`].
    ///
    /// Compaction thresholds are derived from this value, so over-reporting it
    /// means context overflows before anything compacts. Known families are
    /// matched by prefix so dated variants (`claude-sonnet-4-6-20260101`) and
    /// provider-prefixed aliases (`anthropic/claude-sonnet-4-6`) resolve
    /// correctly; anything unrecognised gets a deliberately conservative floor
    /// rather than the largest window we happen to know, so the failure mode is
    /// early compaction rather than a hard context-length error.
    #[must_use]
    pub fn context_window(&self) -> usize {
        Self::context_window_for(&self.0)
    }

    /// Context window for this model, resolved through an explicit override
    /// chain.
    #[must_use]
    pub fn context_window_with(&self, resolver: &ContextWindowResolver) -> usize {
        resolver.resolve(&self.0)
    }

    /// Context window for a raw model name, using the process environment,
    /// client-data file, and built-in table.
    #[must_use]
    pub fn context_window_for(name: &str) -> usize {
        ContextWindowResolver::from_env_and_client_data().resolve(name)
    }

    /// Context window from the built-in model table only, ignoring overrides.
    ///
    /// Useful for diagnostics and for callers that need a pure,
    /// environment-independent answer.
    #[must_use]
    pub fn table_context_window_for(name: &str) -> usize {
        table_context_window(name).unwrap_or(UNKNOWN_CONTEXT_WINDOW)
    }
}

/// Context window for Claude model families.
///
/// Kept at 200k deliberately: it is the conservative floor across the Claude
/// family, including older and gateway-provided variants. Newer models with
/// larger windows can be raised per model through an override.
pub const CLAUDE_CONTEXT_WINDOW: usize = 200_000;

/// Conservative window assumed for models we cannot identify.
///
/// Chosen so that an unknown model compacts early instead of overflowing: a
/// wrong guess that is too small costs some context, a wrong guess that is too
/// large loses the conversation.
pub const UNKNOWN_CONTEXT_WINDOW: usize = 32_000;

/// Global environment variable overriding the context window for every model.
pub const CONTEXT_WINDOW_ENV: &str = "CODEREX_CONTEXT_WINDOW";

/// Prefix for per-model context-window environment variables.
///
/// The model name is upper-cased and every non-alphanumeric character is
/// replaced with `_`, e.g. `claude-sonnet-4-6` becomes
/// `CODEREX_CONTEXT_WINDOW_CLAUDE_SONNET_4_6`.
pub const CONTEXT_WINDOW_MODEL_ENV_PREFIX: &str = "CODEREX_CONTEXT_WINDOW_";

/// File name, relative to the CodeRex config home, for client-data overrides.
pub const CONTEXT_WINDOW_CLIENT_DATA_FILE: &str = "context_windows.json";

/// Built-in per-model context windows, matched by exact name or family prefix.
///
/// Entries are deliberately conservative: a window that is too small costs
/// some context, while one that is too large loses the conversation. Only
/// documented families are listed; everything else falls through to
/// [`UNKNOWN_CONTEXT_WINDOW`]. Order matters — more specific entries must come
/// before their prefixes (`gpt-4o` before `gpt-4`).
pub const MODEL_CONTEXT_WINDOWS: &[(&str, usize)] = &[
    // Anthropic Claude: 200k across the family (conservative floor).
    ("claude", CLAUDE_CONTEXT_WINDOW),
    // OpenAI.
    ("gpt-4o", 128_000),
    ("gpt-4.1", 1_000_000),
    ("gpt-4-turbo", 128_000),
    ("gpt-4", 8_192),
    ("gpt-3.5-turbo", 16_000),
    ("o1-mini", 128_000),
    ("o1", 200_000),
    ("o3", 200_000),
    ("o4-mini", 200_000),
    // Google Gemini.
    ("gemini-2.5-pro", 1_000_000),
    ("gemini-2.5-flash", 1_000_000),
    // DeepSeek.
    ("deepseek-chat", 64_000),
    ("deepseek-reasoner", 64_000),
    ("deepseek-r1", 64_000),
    // Meta Llama.
    ("llama-3.3", 128_000),
    ("llama-3.1", 128_000),
    // Alibaba Qwen.
    ("qwen2.5", 128_000),
    // Mistral.
    ("mistral-large", 128_000),
];

/// Per-model context-window overrides with an optional fallback.
///
/// The same shape is used by settings (`context_window`), client data
/// (`~/.coderex/context_windows.json`), experiments, and the environment
/// parser:
///
/// ```json
/// {
///   "default": 128000,
///   "models": {
///     "my-gateway-alias": 200000
///   }
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextWindowOverrides {
    /// Fallback window for models without a per-model entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<usize>,
    /// Per-model windows, keyed by model name.
    #[serde(default)]
    pub models: HashMap<String, usize>,
}

impl ContextWindowOverrides {
    /// Whether this source contributes no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.models.is_empty()
    }

    /// Resolve a model within this source: a per-model value beats the
    /// source's default. Zero values are ignored.
    #[must_use]
    pub fn resolve(&self, model: &str) -> Option<usize> {
        lookup_model_window(&self.models, model).or_else(|| valid_window(self.default))
    }

    /// Drop blank model keys and zero windows in place.
    pub fn retain_valid(&mut self) {
        self.default = valid_window(self.default);
        self.models
            .retain(|model, tokens| !model.trim().is_empty() && *tokens > 0);
    }

    /// Parse overrides from the process environment.
    ///
    /// Reads [`CONTEXT_WINDOW_ENV`] as the global default and every
    /// `CODEREX_CONTEXT_WINDOW_<SANITIZED_MODEL>` as a per-model override.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_env_vars(std::env::vars())
    }

    /// Parse overrides from an arbitrary key/value iterator.
    ///
    /// This is the testable form of [`Self::from_env`]; invalid and zero
    /// values are ignored so the next source can supply a window.
    #[must_use]
    pub fn from_env_vars<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut overrides = Self::default();
        for (key, value) in vars {
            let key = key.as_ref();
            if key == CONTEXT_WINDOW_ENV {
                overrides.default = parse_window(value.as_ref());
                continue;
            }
            let Some(suffix) = key.strip_prefix(CONTEXT_WINDOW_MODEL_ENV_PREFIX) else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            if let Some(tokens) = parse_window(value.as_ref()) {
                overrides.models.insert(suffix.to_ascii_uppercase(), tokens);
            }
        }
        overrides
    }

    /// Parse the JSON file shape.
    ///
    /// # Errors
    ///
    /// Returns the serde error when the document is not a valid override map.
    pub fn from_json_str(json: &str) -> Result<Self, serde_json::Error> {
        let mut overrides: Self = serde_json::from_str(json)?;
        overrides.retain_valid();
        Ok(overrides)
    }

    /// Load overrides from a JSON file, returning `None` if the file is
    /// missing or invalid.
    #[must_use]
    pub fn from_json_file(path: &Path) -> Option<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        "could not read context window file {}: {error}",
                        path.display()
                    );
                }
                return None;
            }
        };
        match Self::from_json_str(&content) {
            Ok(overrides) => Some(overrides),
            Err(error) => {
                tracing::warn!("invalid context window file {}: {error}", path.display());
                None
            }
        }
    }

    /// Load the client-data override file for the active config home.
    #[must_use]
    pub fn load_client_data() -> Self {
        Self::from_json_file(&context_window_client_data_path()).unwrap_or_default()
    }
}

/// Resolve context windows from an ordered chain of sources.
///
/// Precedence (highest first):
/// 1. environment (per-model, then global)
/// 2. settings (per-model, then default)
/// 3. client data (per-model, then default)
/// 4. experiment/feature flag (per-model, then default)
/// 5. built-in model table
/// 6. [`UNKNOWN_CONTEXT_WINDOW`]
///
/// Source order is authoritative: a global default in a higher-priority source
/// beats a per-model entry in a lower-priority one, while within a single
/// source a per-model entry beats that source's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextWindowResolver {
    /// Process-environment overrides.
    pub env: ContextWindowOverrides,
    /// Settings-file overrides.
    pub settings: ContextWindowOverrides,
    /// Client-data file overrides.
    pub client_data: ContextWindowOverrides,
    /// Experiment/feature-flag overrides.
    pub experiment: ContextWindowOverrides,
}

impl ContextWindowResolver {
    /// Build a resolver from all four override sources.
    #[must_use]
    pub const fn new(
        env: ContextWindowOverrides,
        settings: ContextWindowOverrides,
        client_data: ContextWindowOverrides,
        experiment: ContextWindowOverrides,
    ) -> Self {
        Self {
            env,
            settings,
            client_data,
            experiment,
        }
    }

    /// Build a resolver from the sources core can read on its own: the process
    /// environment and the client-data file.
    ///
    /// Settings and experiment overrides are layered in by the config crate
    /// with [`Self::with_settings`] and [`Self::with_experiment`].
    #[must_use]
    pub fn from_env_and_client_data() -> Self {
        Self {
            env: ContextWindowOverrides::from_env(),
            client_data: ContextWindowOverrides::load_client_data(),
            ..Self::default()
        }
    }

    /// Attach settings overrides.
    #[must_use]
    pub fn with_settings(mut self, settings: ContextWindowOverrides) -> Self {
        self.settings = settings;
        self
    }

    /// Attach experiment/feature-flag overrides.
    #[must_use]
    pub fn with_experiment(mut self, experiment: ContextWindowOverrides) -> Self {
        self.experiment = experiment;
        self
    }

    /// Resolve the window for a model name.
    #[must_use]
    pub fn resolve(&self, model: &str) -> usize {
        for source in [
            &self.env,
            &self.settings,
            &self.client_data,
            &self.experiment,
        ] {
            if let Some(window) = source.resolve(model) {
                return window;
            }
        }
        table_context_window(model).unwrap_or(UNKNOWN_CONTEXT_WINDOW)
    }
}

/// Path to the client-data context-window file.
///
/// Uses `CLAUDE_CONFIG_DIR` when set, otherwise `$HOME/.coderex`, matching
/// [`crate`]'s config home convention. Falls back to `.coderex` when no home
/// directory can be determined.
#[must_use]
pub fn context_window_client_data_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir).join(CONTEXT_WINDOW_CLIENT_DATA_FILE);
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home)
            .join(".coderex")
            .join(CONTEXT_WINDOW_CLIENT_DATA_FILE);
    }
    PathBuf::from(".coderex").join(CONTEXT_WINDOW_CLIENT_DATA_FILE)
}

/// Normalize a model name for lookup and matching.
///
/// Lower-cases the name, drops a trailing `[1m]` marker, and strips common
/// provider/router prefixes (`models/`, `anthropic/`, `openai/`,
/// `bedrock/`, regional `us.`/`eu.` prefixes, and similar). The result is
/// what the table matcher and normalized map lookups compare against.
#[must_use]
pub fn normalized_model_name(name: &str) -> String {
    const PREFIXES: &[&str] = &[
        "models/",
        "anthropic/",
        "anthropic.",
        "openai/",
        "openai.",
        "google/",
        "google.",
        "vertex/",
        "bedrock/",
        "openrouter/",
        "azure/",
        "aws/",
        "meta/",
        "meta-llama/",
        "deepseek/",
        "mistral/",
        "qwen/",
        "x-ai/",
        "us.",
        "eu.",
        "apac.",
    ];

    let mut normalized = name.trim().to_ascii_lowercase();
    if let Some(stripped) = normalized.strip_suffix("[1m]") {
        normalized = stripped.trim_end().to_string();
    }
    loop {
        let before = normalized.clone();
        for prefix in PREFIXES {
            if let Some(rest) = normalized.strip_prefix(prefix) {
                normalized = rest.to_string();
            }
        }
        if normalized == before {
            break;
        }
    }
    normalized
}

/// Sanitize a model name for use in an environment variable suffix.
///
/// Upper-cases the name and replaces every non-alphanumeric character with
/// `_`. Distinct names can collide (for example `a.b` and `a-b`), so the
/// per-model environment form is best reserved for conventional model ids.
#[must_use]
pub fn sanitized_model_env_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_window(value: &str) -> Option<usize> {
    value
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|tokens| *tokens > 0)
}

fn valid_window(window: Option<usize>) -> Option<usize> {
    window.filter(|tokens| *tokens > 0)
}

fn model_name_candidates(name: &str) -> Vec<String> {
    let trimmed = name.trim();
    let mut candidates = Vec::with_capacity(4);
    if trimmed.is_empty() {
        return candidates;
    }
    candidates.push(trimmed.to_string());
    let lower = trimmed.to_ascii_lowercase();
    if !candidates.contains(&lower) {
        candidates.push(lower);
    }
    let normalized = normalized_model_name(trimmed);
    if !candidates.contains(&normalized) {
        candidates.push(normalized);
    }
    candidates
}

fn lookup_model_window(models: &HashMap<String, usize>, name: &str) -> Option<usize> {
    if models.is_empty() {
        return None;
    }

    // Exact and case-normalized candidates first, then the sanitized form used
    // by per-model environment variables (which stores upper-case keys).
    for candidate in model_name_candidates(name) {
        if let Some(window) = models.get(&candidate).and_then(|v| valid_window(Some(*v))) {
            return Some(window);
        }
        let sanitized = sanitized_model_env_name(&candidate);
        if let Some(window) = models.get(&sanitized).and_then(|v| valid_window(Some(*v))) {
            return Some(window);
        }
    }

    // Then compare fully normalized keys, so a map key written as
    // `anthropic/claude-sonnet-4-6` still matches `claude-sonnet-4-6`.
    let target = normalized_model_name(name);
    let mut matches: Vec<(&String, usize)> = models
        .iter()
        .filter(|(key, _)| normalized_model_name(key) == target)
        .filter_map(|(key, tokens)| valid_window(Some(*tokens)).map(|tokens| (key, tokens)))
        .collect();
    matches.sort_by(|left, right| left.0.cmp(right.0));
    matches.first().map(|(_, tokens)| *tokens)
}

fn table_context_window(name: &str) -> Option<usize> {
    let normalized = normalized_model_name(name);
    MODEL_CONTEXT_WINDOWS
        .iter()
        .find(|(prefix, _)| {
            normalized == *prefix
                || normalized
                    .strip_prefix(*prefix)
                    .is_some_and(|rest| rest.starts_with('-'))
        })
        .map(|(_, window)| *window)
}

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
    /// A one-line, model-written description of a tool use.
    ///
    /// Emitted only when the `tool_use_summaries` flag is on, before the
    /// matching [`ToolCallStart`](Self::ToolCallStart). The UI can use it as
    /// the display label; the engine falls back to
    /// [`ToolCall::activity_summary`](crate::types::ToolCall::activity_summary)
    /// when summarization is unavailable, so this event is advisory.
    ToolUseSummary {
        /// The tool call this summary describes.
        id: String,
        /// Tool name.
        name: String,
        /// One-line summary, already trimmed to a single line.
        summary: String,
    },
    /// A compact boundary marker was inserted.
    CompactBoundary,
    /// Compaction was requested, ran, or stood down, and why.
    ///
    /// [`CompactBoundary`](Self::CompactBoundary) says a marker was written;
    /// this says *why* compaction happened or did not, which a caller cannot
    /// read off a boundary and a test cannot read off a log line.
    Compaction(CompactionEvent),
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

/// A compaction lifecycle event, carried by [`EngineEvent::Compaction`].
///
/// This is the wire-shaped mirror of the engine's `CompactOutcome`, plus the
/// timer's *request*: the strategies and triggers are named as strings so the
/// frame survives a round trip through the wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactionEvent {
    /// The idle cache-TTL timer asked the session to compact.
    ///
    /// This is a request, not an outcome: the timer cannot compact an idle
    /// session by itself, so it says why compaction would be worth running.
    Requested {
        /// What asked for it (`cache_ttl`).
        trigger: String,
    },
    /// Compaction ran and removed tokens.
    Compacted {
        /// Which strategy ran.
        strategy: String,
        /// What triggered it.
        trigger: String,
        /// Estimated tokens removed.
        tokens_removed: usize,
        /// Messages left after compaction.
        messages_remaining: usize,
    },
    /// The check ran but this trigger stood down: the other path owns it.
    DeferredToThresholdPath {
        /// Estimated tokens at the time.
        estimated_tokens: usize,
    },
    /// The recovery retry budget is spent.
    SkippedRecoveryBudget {
        /// Recovery attempts already made.
        recovery_count: u32,
        /// The limit.
        max: u32,
    },
    /// Auto-compaction is paused after consecutive failures.
    CircuitBreakerOpen {
        /// Consecutive failures so far.
        consecutive_failures: u32,
        /// The limit.
        max: u32,
    },
    /// The Stop hook ended the turn instead of compacting.
    StopHookTerminated {
        /// The hook's reason.
        reason: String,
    },
    /// Compaction ran but removed nothing.
    NoProgress {
        /// Tokens before.
        tokens_before: usize,
        /// Tokens after.
        tokens_after: usize,
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
            images: Vec::new(),
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
            images: Vec::new(),
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

    fn overrides(default: Option<usize>, models: &[(&str, usize)]) -> ContextWindowOverrides {
        ContextWindowOverrides {
            default,
            models: models
                .iter()
                .map(|(model, tokens)| ((*model).to_string(), *tokens))
                .collect(),
        }
    }

    #[test]
    fn known_families_report_their_full_window() {
        assert_eq!(
            ModelId::table_context_window_for("claude-opus-4-6"),
            CLAUDE_CONTEXT_WINDOW
        );
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

    #[test]
    fn normalization_handles_case_and_provider_prefixes() {
        for name in [
            "CLAUDE-SONNET-4-6",
            "anthropic/claude-sonnet-4-6",
            "models/claude-sonnet-4-6",
            "us.anthropic.claude-sonnet-4-6",
            "claude-sonnet-4-6[1m]",
        ] {
            assert_eq!(
                ModelId::table_context_window_for(name),
                CLAUDE_CONTEXT_WINDOW,
                "{name} should normalize before matching"
            );
        }
    }

    #[test]
    fn model_table_has_conservative_entries_for_common_gateways() {
        for (name, expected) in [
            ("gpt-4o", 128_000),
            ("gpt-4o-2024-08-06", 128_000),
            ("gpt-4.1-mini", 1_000_000),
            ("gemini-2.5-pro", 1_000_000),
            ("deepseek-chat", 64_000),
            ("llama-3.3-70b-instruct", 128_000),
            ("qwen2.5-72b-instruct", 128_000),
            ("mistral-large-latest", 128_000),
        ] {
            assert_eq!(ModelId::table_context_window_for(name), expected, "{name}");
        }
    }

    /// The bug this guards: an unknown model previously reported 200k, so
    /// compaction would not trigger until ~180k tokens.
    #[test]
    fn unknown_models_are_conservative_not_maximal() {
        for name in ["local-llama-3", "my-gateway-alias", "totally-unknown"] {
            let window = ModelId::table_context_window_for(name);
            assert_eq!(window, UNKNOWN_CONTEXT_WINDOW, "{name}");
            assert!(
                window < CLAUDE_CONTEXT_WINDOW,
                "unknown models must not claim the largest known window"
            );
        }
    }

    #[test]
    fn per_model_env_name_is_sanitized() {
        assert_eq!(
            sanitized_model_env_name("claude-sonnet-4-6"),
            "CLAUDE_SONNET_4_6"
        );
        assert_eq!(sanitized_model_env_name("gpt-4o"), "GPT_4O");
        assert_eq!(
            sanitized_model_env_name("vendor/model.v2"),
            "VENDOR_MODEL_V2"
        );
    }

    #[test]
    fn env_per_model_beats_env_global() {
        let env = ContextWindowOverrides::from_env_vars([
            (CONTEXT_WINDOW_ENV, "64000"),
            ("CODEREX_CONTEXT_WINDOW_MY_GATEWAY", "128000"),
        ]);
        let resolver = ContextWindowResolver::new(
            env,
            Default::default(),
            Default::default(),
            Default::default(),
        );

        assert_eq!(resolver.resolve("my-gateway"), 128_000);
        assert_eq!(resolver.resolve("some-other-model"), 64_000);
    }

    #[test]
    fn env_matches_provider_prefixed_and_case_variant_model_names() {
        let env = ContextWindowOverrides::from_env_vars([(
            "CODEREX_CONTEXT_WINDOW_CLAUDE_SONNET_4_6",
            "500000",
        )]);
        let resolver = ContextWindowResolver::new(
            env,
            Default::default(),
            Default::default(),
            Default::default(),
        );

        for name in [
            "claude-sonnet-4-6",
            "CLAUDE-SONNET-4-6",
            "anthropic/claude-sonnet-4-6",
        ] {
            assert_eq!(resolver.resolve(name), 500_000, "{name}");
        }
    }

    /// Source order is authoritative: a higher-priority global default beats a
    /// lower-priority per-model value, while a per-model value beats the global
    /// default inside the same source.
    #[test]
    fn override_chain_precedence_at_every_level() {
        let model = "gateway-alias";

        let resolver = ContextWindowResolver::new(
            overrides(Some(100_000), &[(model, 110_000)]),
            overrides(Some(200_000), &[(model, 210_000)]),
            overrides(Some(300_000), &[(model, 310_000)]),
            overrides(Some(400_000), &[(model, 410_000)]),
        );
        assert_eq!(resolver.resolve(model), 110_000, "env wins");

        let resolver = ContextWindowResolver::new(
            Default::default(),
            overrides(Some(200_000), &[(model, 210_000)]),
            overrides(Some(300_000), &[(model, 310_000)]),
            overrides(Some(400_000), &[(model, 410_000)]),
        );
        assert_eq!(resolver.resolve(model), 210_000, "settings wins");

        let resolver = ContextWindowResolver::new(
            Default::default(),
            Default::default(),
            overrides(Some(300_000), &[(model, 310_000)]),
            overrides(Some(400_000), &[(model, 410_000)]),
        );
        assert_eq!(resolver.resolve(model), 310_000, "client data wins");

        let resolver = ContextWindowResolver::new(
            Default::default(),
            Default::default(),
            Default::default(),
            overrides(Some(400_000), &[(model, 410_000)]),
        );
        assert_eq!(resolver.resolve(model), 410_000, "experiment wins");

        let resolver = ContextWindowResolver::default();
        assert_eq!(
            resolver.resolve("claude-sonnet-4-6"),
            CLAUDE_CONTEXT_WINDOW,
            "table is the final known source"
        );
        assert_eq!(
            resolver.resolve(model),
            UNKNOWN_CONTEXT_WINDOW,
            "unknown falls back conservatively"
        );
    }

    #[test]
    fn global_default_beats_a_lower_source_per_model_value() {
        let resolver = ContextWindowResolver::new(
            overrides(Some(90_000), &[]),
            overrides(None, &[("gateway-alias", 210_000)]),
            Default::default(),
            Default::default(),
        );
        assert_eq!(
            resolver.resolve("gateway-alias"),
            90_000,
            "env global default outranks a settings per-model value"
        );
    }

    #[test]
    fn invalid_and_zero_values_fall_through() {
        let resolver = ContextWindowResolver::new(
            overrides(Some(0), &[("gateway-alias", 0)]),
            overrides(Some(0), &[("gateway-alias", 0)]),
            overrides(Some(0), &[("gateway-alias", 0)]),
            overrides(Some(0), &[("gateway-alias", 0)]),
        );
        assert_eq!(resolver.resolve("gateway-alias"), UNKNOWN_CONTEXT_WINDOW);

        let env = ContextWindowOverrides::from_env_vars([
            (CONTEXT_WINDOW_ENV, "not-a-number"),
            ("CODEREX_CONTEXT_WINDOW_MY_GATEWAY", "0"),
        ]);
        assert!(env.is_empty());

        let resolver = ContextWindowResolver::new(
            env,
            overrides(Some(0), &[("gateway-alias", 0)]),
            Default::default(),
            overrides(Some(123_000), &[]),
        );
        assert_eq!(
            resolver.resolve("gateway-alias"),
            123_000,
            "a valid lower source is used after invalid higher ones"
        );
    }

    #[test]
    fn client_data_json_uses_the_documented_shape() {
        let overrides = ContextWindowOverrides::from_json_str(
            r#"{
                "default": 64000,
                "models": {
                    "my-gateway-alias": 128000,
                    "claude-sonnet-4-6": 200000,
                    "zero-is-ignored": 0
                }
            }"#,
        )
        .unwrap();

        assert_eq!(overrides.resolve("my-gateway-alias"), Some(128_000));
        assert_eq!(overrides.resolve("claude-sonnet-4-6"), Some(200_000));
        // The zero entry is dropped, so the model falls through to the
        // source's default rather than disabling compaction.
        assert_eq!(overrides.resolve("zero-is-ignored"), Some(64_000));
        assert_eq!(overrides.resolve("other"), Some(64_000));
    }

    #[test]
    fn client_data_map_matches_normalized_keys() {
        let overrides = overrides(None, &[("anthropic/claude-sonnet-4-6", 210_000)]);
        assert_eq!(
            overrides.resolve("claude-sonnet-4-6"),
            Some(210_000),
            "provider-prefixed map keys still match the bare model name"
        );
    }
}
