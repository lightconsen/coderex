//! Feature flags and capability sets.

use bitflags::bitflags;
use coderex_core::types::ContextWindowOverrides;
use std::collections::HashMap;
use std::fmt;

/// Every feature flag name understood by [`FeatureFlags`].
///
/// Kept in one place so `--betas` can reject an unknown name with the full
/// list rather than a shrug, and so the parser and the docs cannot drift.
pub const KNOWN_FLAGS: &[&str] = &[
    "streaming_tool_execution",
    "tool_use_summaries",
    "fast_mode",
    "coordinator_mode",
    "history_snip",
    "agent_triggers",
    "voice_mode",
    "workflow_scripts",
    "web_browser_tool",
    "team_collaboration",
    "auto_dream",
    // Commands whose availability is decided by a flag. They are separate
    // from the runtime capabilities above: enabling one only makes its slash
    // command executable, it does not change the engine's capabilities.
    "ultraplan",
    "thinkback",
    "heapdump",
    "mock_limits",
    "ctx_viz",
    "stickers",
];

/// Flags accepted for configuration compatibility that have no effect in
/// this build.
///
/// A name belongs here only when it stays in [`KNOWN_FLAGS`] so configuration
/// written by another build still parses, yet no production code reads it.
/// [`FeatureFlags`] stores the state and [`FeatureFlags::inert_flags`] reports
/// it. The list is empty: every name that used to be here has either been
/// wired to a product path or deleted.
///
/// Wiring history for the six names this list last held:
///
/// - `streaming_tool_execution` is now the switch between executing tool calls
///   as they arrive in a streamed response (`execute_early_tool_call`) and
///   executing them after the stream completes. It is off by default, matching
///   the reference; the CLI installs it on the session engine.
/// - `tool_use_summaries` now makes the engine ask a
///   `ModelProfile::Lightweight` model for a one-line summary of each tool use
///   and emit it as [`coderex_core::types::EngineEvent::ToolUseSummary`],
///   falling back to `ToolCall::activity_summary()` when the summarizer fails.
/// - `team_collaboration` now makes the CLI install a
///   `core::team_context::TeamContext` on the session engine, which reads its
///   notes and activity into the request context each turn.
/// - `bridge_mode`, `daemon`, and `buddy` were deleted. The bridge and the
///   daemon are separate CLI entry points (`bridge`, `serve`), which a session
///   flag cannot gate; `buddy` lives behind the compile-time `buddy` cargo
///   feature, which a runtime flag cannot reach.
///
/// A name is deleted outright when no code in the workspace could ever be its
/// product surface. `context_collapse`, `terminal_panel`, and `kairos` were
/// removed for that reason, and now so were `bridge_mode`, `daemon`, and
/// `buddy`.
const INERT_FLAGS: &[&str] = &[];

/// One or more feature flag names that this build does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFeatureFlags {
    /// The names that were rejected, in the order they appeared.
    pub names: Vec<String>,
}

impl fmt::Display for UnknownFeatureFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown feature flag(s): {} (known flags: {})",
            self.names.join(", "),
            KNOWN_FLAGS.join(", ")
        )
    }
}

impl std::error::Error for UnknownFeatureFlags {}

/// All feature flags supported by CodeRex.
#[derive(Debug, Clone, Default)]
pub struct FeatureFlags {
    /// Execute tool calls as they arrive in a streamed response.
    ///
    /// On: a completed tool call is executed while the stream is still open.
    /// Off (the default, matching the reference): calls execute after the
    /// stream completes. The CLI installs this on the session engine.
    pub streaming_tool_execution: bool,
    /// Ask the lightweight model for a one-line summary of each tool use.
    ///
    /// When on, the engine emits an
    /// [`EngineEvent::ToolUseSummary`](coderex_core::types::EngineEvent::ToolUseSummary)
    /// before each tool call and uses that summary as the activity label. A
    /// failed summarization falls back to `ToolCall::activity_summary()`.
    pub tool_use_summaries: bool,
    /// Run the session on the lightweight model profile.
    ///
    /// When on and no model was explicitly configured (`--model`,
    /// `ANTHROPIC_MODEL`/`OPENAI_MODEL`, or a settings `model` value), the
    /// session model resolves through `ModelProfile::Lightweight`.
    pub fast_mode: bool,
    /// Make the main agent coordinate sub-agents.
    ///
    /// When on, the CLI installs a `CoordinatorState` on the session engine, so
    /// its per-turn phase advance runs.
    pub coordinator_mode: bool,
    /// Snip history at a token boundary.
    ///
    /// When on, compaction prefers the `Snip` strategy as soon as it is
    /// warranted rather than only when the context window is spent.
    pub history_snip: bool,
    /// Enable agent triggers.
    pub agent_triggers: bool,
    /// Enable voice mode.
    pub voice_mode: bool,
    /// Enable workflow scripts.
    pub workflow_scripts: bool,
    /// Enable web browser tool.
    pub web_browser_tool: bool,
    /// Enable team collaboration.
    ///
    /// When on, the CLI installs a `TeamContext` on the session engine, which
    /// reads its shared notes, in-progress work and recent activity into the
    /// request context each turn.
    pub team_collaboration: bool,
    /// Enable auto dream mode.
    pub auto_dream: bool,
    /// Enable the `/ultraplan` command.
    pub ultraplan: bool,
    /// Enable the `/thinkback` and `/thinkback-play` commands.
    pub thinkback: bool,
    /// Enable the `/heapdump` debug command.
    pub heapdump: bool,
    /// Enable the `/mock-limits` debug command.
    pub mock_limits: bool,
    /// Enable the `/ctx_viz` command.
    pub ctx_viz: bool,
    /// Enable the `/stickers` command.
    pub stickers: bool,
    /// Enable experiment-supplied context-window overrides.
    ///
    /// The window values themselves live in [`Self::context_window_default`]
    /// and [`Self::context_windows`]; this flag gates whether they participate
    /// in resolution.
    pub context_window_overrides: bool,
    /// Experiment-supplied fallback context window, in tokens.
    pub context_window_default: Option<usize>,
    /// Experiment-supplied per-model context windows, in tokens.
    pub context_windows: HashMap<String, usize>,
}

bitflags! {
    /// The set of capabilities available at runtime, derived from FeatureFlags + runtime conditions.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CapabilitySet: u64 {
        /// Streaming tool execution capability.
        const STREAMING_TOOLS = 1 << 0;
        /// Coordinator mode capability.
        const COORDINATOR = 1 << 1;
        /// Voice mode capability.
        const VOICE = 1 << 2;
        /// Workflow scripts capability.
        const WORKFLOW = 1 << 3;
        /// Web browser tool capability.
        const WEB_BROWSER = 1 << 4;
        /// Team collaboration capability.
        const TEAM = 1 << 5;
        /// Dream mode capability.
        const DREAM = 1 << 6;
    }
}

impl FeatureFlags {
    /// Parse feature flags from a comma-separated string.
    ///
    /// Format: `+flag_name` enables, `-flag_name` disables. Bare names default to enable.
    /// Example: `"+streaming_tool_execution,-voice_mode"`
    ///
    /// Unknown names are logged and skipped. Use [`Self::parse_checked`] when
    /// a person supplied the list and a typo must be reported rather than
    /// ignored.
    ///
    /// Context-window experiment values use assignment tokens:
    /// `context_window_default=64000`, `context_window=my-alias:128000`, or
    /// `context_window.my-alias=128000`. A valid value also enables
    /// `context_window_overrides`.
    #[must_use]
    pub fn parse(input: &str) -> Self {
        let mut flags = Self::default();
        // The error is intentional here: `apply_comma_list` logs each unknown
        // name, and `parse` keeps its infallible signature for callers that
        // treat flags as best-effort configuration (the environment variable).
        let _ = flags.apply_comma_list(input);
        flags
    }

    /// Parse feature flags, returning the names that were not understood.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownFeatureFlags`] listing every unknown name. Known names
    /// are still applied before the error is returned.
    pub fn parse_checked(input: &str) -> Result<Self, UnknownFeatureFlags> {
        let mut flags = Self::default();
        flags.apply_comma_list(input)?;
        Ok(flags)
    }

    /// Apply a comma-separated flag list on top of the current flags.
    ///
    /// Accepts `+name`, `-name`, and bare `name` (enable). Empty tokens are
    /// ignored. If any name is unknown, the known ones are still applied and
    /// every unknown name is returned.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownFeatureFlags`] when at least one token is not a flag
    /// this build supports.
    pub fn apply_comma_list(&mut self, input: &str) -> Result<(), UnknownFeatureFlags> {
        let mut unknown = Vec::new();
        for token in input.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Some((name, value)) = token.split_once('=') {
                self.apply_value_flag(name.trim(), value.trim());
                continue;
            }
            let (enable, name) = match token.chars().next() {
                Some('+') => (true, &token[1..]),
                Some('-') => (false, &token[1..]),
                _ => (true, token),
            };
            if name.is_empty() || !self.apply_flag(name, enable) {
                unknown.push(name.to_string());
            }
        }
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(UnknownFeatureFlags { names: unknown })
        }
    }

    /// Parse feature flags from the `CODEREX_FEATURE_FLAGS` environment variable.
    ///
    /// Delegates to `from_str` with the env var value.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var("CODEREX_FEATURE_FLAGS")
            .map_or_else(|_| Self::default(), |val| Self::parse(&val))
    }

    /// Derive the CapabilitySet from feature flags.
    #[must_use]
    pub fn capabilities(&self) -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        if self.streaming_tool_execution {
            caps.insert(CapabilitySet::STREAMING_TOOLS);
        }
        if self.coordinator_mode {
            caps.insert(CapabilitySet::COORDINATOR);
        }
        if self.voice_mode {
            caps.insert(CapabilitySet::VOICE);
        }
        if self.workflow_scripts {
            caps.insert(CapabilitySet::WORKFLOW);
        }
        if self.web_browser_tool {
            caps.insert(CapabilitySet::WEB_BROWSER);
        }
        if self.team_collaboration {
            caps.insert(CapabilitySet::TEAM);
        }
        if self.auto_dream {
            caps.insert(CapabilitySet::DREAM);
        }
        caps
    }

    /// The enabled flags from the compatibility-only set, in
    /// [`KNOWN_FLAGS`] order.
    ///
    /// These names are accepted for configuration compatibility but have no
    /// effect in this build: no production code reads them, and enabling one
    /// changes no behavior. They are reported here so tooling and tests can
    /// distinguish a flag that is merely stored from one that does something,
    /// without removing the names from configuration files that mention them.
    /// The list is currently empty; see [`INERT_FLAGS`] for the history.
    #[must_use]
    pub fn inert_flags(&self) -> Vec<&'static str> {
        INERT_FLAGS
            .iter()
            .copied()
            .filter(|name| self.is_enabled(name))
            .collect()
    }

    /// Every flag as a name → enabled map.
    ///
    /// The slash-command context carries flags as a map, because commands are
    /// gated by the name they ask for rather than by a typed field. Keeping
    /// this conversion here means a name added to [`KNOWN_FLAGS`] cannot be
    /// silently missing at the call site.
    #[must_use]
    pub fn as_map(&self) -> HashMap<String, bool> {
        KNOWN_FLAGS
            .iter()
            .map(|name| ((*name).to_string(), self.is_enabled(name)))
            .collect()
    }

    /// Whether the named flag is on. Unknown names are off.
    #[must_use]
    pub fn is_enabled(&self, name: &str) -> bool {
        match name {
            "streaming_tool_execution" => self.streaming_tool_execution,
            "tool_use_summaries" => self.tool_use_summaries,
            "fast_mode" => self.fast_mode,
            "coordinator_mode" => self.coordinator_mode,
            "history_snip" => self.history_snip,
            "agent_triggers" => self.agent_triggers,
            "voice_mode" => self.voice_mode,
            "workflow_scripts" => self.workflow_scripts,
            "web_browser_tool" => self.web_browser_tool,
            "team_collaboration" => self.team_collaboration,
            "auto_dream" => self.auto_dream,
            "ultraplan" => self.ultraplan,
            "thinkback" => self.thinkback,
            "heapdump" => self.heapdump,
            "mock_limits" => self.mock_limits,
            "ctx_viz" => self.ctx_viz,
            "stickers" => self.stickers,
            "context_window_overrides" => self.context_window_overrides,
            _ => false,
        }
    }

    /// Convenience method to create flags from env and derive capabilities.
    #[must_use]
    pub fn from_flags_and_env() -> (Self, CapabilitySet) {
        let flags = Self::from_env();
        let caps = flags.capabilities();
        (flags, caps)
    }

    /// Apply remote experiment overrides on top of base flags.
    #[must_use]
    pub fn with_experiments(&self, experiments: &HashMap<String, bool>) -> Self {
        let mut flags = self.clone();
        for (name, &enabled) in experiments {
            flags.apply_flag(name, enabled);
        }
        flags
    }

    /// Apply per-project flag overrides on top of base flags.
    ///
    /// Project-level flags take precedence over global flags for the current
    /// project context (based on `.coderex/flags.json` in the project root).
    #[must_use]
    pub fn with_project_overrides(&self, project_overrides: &HashMap<String, bool>) -> Self {
        let mut flags = self.clone();
        for (name, &enabled) in project_overrides {
            flags.apply_flag(name, enabled);
        }
        flags
    }

    /// Load per-project feature flags from a JSON file at the given path.
    ///
    /// Returns an empty map if the file doesn't exist or can't be parsed.
    /// The file format is `{"voice_mode": true, "workflow_scripts": false, ...}`.
    pub fn load_project_flags(path: &std::path::Path) -> HashMap<String, bool> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_json::from_str::<HashMap<String, bool>>(&content).ok())
            .unwrap_or_default()
    }

    /// Context-window overrides supplied by experiments.
    ///
    /// Returns an empty override set unless [`Self::context_window_overrides`]
    /// is enabled, so experiment data only participates when the experiment
    /// itself is active. Invalid values are dropped so resolution falls through
    /// to the next source.
    #[must_use]
    pub fn experiment_context_windows(&self) -> ContextWindowOverrides {
        if !self.context_window_overrides {
            return ContextWindowOverrides::default();
        }
        let models = self
            .context_windows
            .iter()
            .filter(|(model, tokens)| !model.trim().is_empty() && **tokens > 0)
            .map(|(model, tokens)| (model.clone(), *tokens))
            .collect();
        ContextWindowOverrides {
            default: self.context_window_default.filter(|tokens| *tokens > 0),
            models,
        }
    }

    fn apply_value_flag(&mut self, name: &str, value: &str) {
        match name {
            "context_window_default" => {
                if let Some(tokens) = parse_context_window_tokens(value) {
                    self.context_window_default = Some(tokens);
                    self.context_window_overrides = true;
                }
            }
            "context_window" => {
                // `context_window=<model>:<tokens>`. Split from the right so
                // Bedrock version suffixes (`...-v1:0`) stay part of the name.
                if let Some((model, tokens)) = value.rsplit_once(':') {
                    if let Some(tokens) = parse_context_window_tokens(tokens) {
                        self.set_context_window(model, tokens);
                    }
                } else {
                    tracing::warn!("invalid context_window experiment value: {value}");
                }
            }
            other if other.starts_with("context_window.") => {
                let model = &other["context_window.".len()..];
                if let Some(tokens) = parse_context_window_tokens(value) {
                    self.set_context_window(model, tokens);
                }
            }
            other => tracing::warn!("unknown feature flag: {other}"),
        }
    }

    fn set_context_window(&mut self, model: &str, tokens: usize) {
        let model = model.trim();
        if model.is_empty() {
            tracing::warn!("context_window experiment entry has an empty model name");
            return;
        }
        self.context_windows.insert(model.to_string(), tokens);
        self.context_window_overrides = true;
    }

    /// Set one known flag. Returns `false` for a name this build does not know.
    fn apply_flag(&mut self, name: &str, enable: bool) -> bool {
        match name {
            "streaming_tool_execution" => self.streaming_tool_execution = enable,
            "tool_use_summaries" => self.tool_use_summaries = enable,
            "fast_mode" => self.fast_mode = enable,
            "coordinator_mode" => self.coordinator_mode = enable,
            "history_snip" => self.history_snip = enable,
            "agent_triggers" => self.agent_triggers = enable,
            "voice_mode" => self.voice_mode = enable,
            "workflow_scripts" => self.workflow_scripts = enable,
            "web_browser_tool" => self.web_browser_tool = enable,
            "team_collaboration" => self.team_collaboration = enable,
            "auto_dream" => self.auto_dream = enable,
            "ultraplan" => self.ultraplan = enable,
            "thinkback" => self.thinkback = enable,
            "heapdump" => self.heapdump = enable,
            "mock_limits" => self.mock_limits = enable,
            "ctx_viz" => self.ctx_viz = enable,
            "stickers" => self.stickers = enable,
            "context_window_overrides" => self.context_window_overrides = enable,
            _ => {
                tracing::warn!("unknown feature flag: {name}");
                return false;
            }
        }
        true
    }
}

fn parse_context_window_tokens(value: &str) -> Option<usize> {
    value
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|tokens| *tokens > 0)
}

/// Gate a block of code behind a compile-time feature flag.
#[macro_export]
macro_rules! feature_gate {
    ($feature:literal, $expr:expr) => {
        if cfg!(feature = $feature) {
            Some($expr)
        } else {
            None
        }
    };
}

/// Thread-safe registry for runtime feature flag queries.
///
/// Wraps [`FeatureFlags`] in a read-write lock so multiple components
/// can query and update flags concurrently.
#[derive(Debug, Clone, Default)]
pub struct FlagRegistry {
    flags: std::sync::Arc<std::sync::RwLock<FeatureFlags>>,
}

impl FlagRegistry {
    /// Create a new registry with default flags.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry from parsed flags.
    #[must_use]
    pub fn from_flags(flags: FeatureFlags) -> Self {
        Self {
            flags: std::sync::Arc::new(std::sync::RwLock::new(flags)),
        }
    }

    /// Load flags from environment and create a registry.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_flags(FeatureFlags::from_env())
    }

    /// Get a snapshot of current flags.
    #[must_use]
    pub fn flags(&self) -> FeatureFlags {
        self.flags.read().unwrap().clone()
    }

    /// Get current capabilities.
    #[must_use]
    pub fn capabilities(&self) -> CapabilitySet {
        self.flags.read().unwrap().capabilities()
    }

    /// Check if a specific capability is enabled.
    #[must_use]
    pub fn has_capability(&self, cap: CapabilitySet) -> bool {
        self.capabilities().contains(cap)
    }

    /// Update a flag by name.
    pub fn set_flag(&self, name: &str, enable: bool) {
        let mut flags = self.flags.write().unwrap();
        flags.apply_flag(name, enable);
    }

    /// Apply experiment overrides.
    pub fn apply_experiments(&self, experiments: &HashMap<String, bool>) {
        let mut flags = self.flags.write().unwrap();
        for (name, &enabled) in experiments {
            flags.apply_flag(name, enabled);
        }
    }

    /// Apply a per-project feature flag override.
    pub fn set_project_override(&self, name: &str, enable: bool) {
        let mut flags = self.flags.write().unwrap();
        flags.apply_flag(name, enable);
    }

    /// Load and apply per-project flags from a JSON file.
    ///
    /// This is the primary way to apply project-specific feature flags
    /// that override global settings for the current project context.
    pub fn load_project_flags(&self, path: &std::path::Path) {
        let project_flags = FeatureFlags::load_project_flags(path);
        let mut flags = self.flags.write().unwrap();
        for (name, enabled) in project_flags {
            flags.apply_flag(&name, enabled);
        }
    }
}

/// Determine if a feature should be enabled for this session based on
/// a percentage rollout (0–100).
///
/// Uses a hash of the feature name + session identifier for stable
/// assignment across restarts.
#[must_use]
pub fn percentage_rollout(feature_name: &str, session_id: &str, percentage: u8) -> bool {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    feature_name.hash(&mut hasher);
    session_id.hash(&mut hasher);
    let hash = hasher.finish();

    (hash % 100) < u64::from(percentage.clamp(0, 100))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn feature_flags_from_parse_enables_flag() {
        let flags = FeatureFlags::parse("voice_mode");
        assert!(flags.voice_mode);
        assert!(!flags.workflow_scripts);
    }

    #[test]
    fn feature_flags_from_parse_enables_and_disables() {
        let flags =
            FeatureFlags::parse("+voice_mode,+streaming_tool_execution,-streaming_tool_execution");
        assert!(flags.voice_mode);
        assert!(!flags.streaming_tool_execution);
    }

    #[test]
    fn feature_flags_unknown_flag_no_panic() {
        let flags = FeatureFlags::parse("nonexistent_flag");
        assert!(!flags.voice_mode);
    }

    #[test]
    fn checked_parse_reports_every_unknown_name() {
        let error = FeatureFlags::parse_checked("+voice_mode,not_a_flag,-also_missing")
            .expect_err("unknown flags must be reported");
        assert_eq!(error.names, vec!["not_a_flag", "also_missing"]);
        let rendered = error.to_string();
        assert!(rendered.contains("not_a_flag"), "{rendered}");
        assert!(rendered.contains("known flags:"), "{rendered}");
        assert!(rendered.contains("voice_mode"), "{rendered}");
    }

    #[test]
    fn checked_parse_accepts_plus_minus_and_bare_names() {
        let flags = FeatureFlags::parse_checked("+voice_mode,-workflow_scripts,history_snip")
            .expect("all three spellings are valid");
        assert!(flags.voice_mode);
        assert!(!flags.workflow_scripts);
        assert!(flags.history_snip);
    }

    #[test]
    fn apply_comma_list_overlays_existing_flags() {
        let mut flags = FeatureFlags {
            voice_mode: true,
            workflow_scripts: true,
            ..Default::default()
        };

        flags
            .apply_comma_list("-voice_mode,+history_snip")
            .expect("known flags");
        assert!(!flags.voice_mode, "a '-' token must disable");
        assert!(flags.workflow_scripts, "untouched flags keep their value");
        assert!(flags.history_snip, "a '+' token must enable");
    }

    #[test]
    fn apply_comma_list_applies_known_tokens_even_with_an_unknown_one() {
        let mut flags = FeatureFlags::default();
        let error = flags
            .apply_comma_list("voice_mode,nope")
            .expect_err("the typo must be reported");
        assert_eq!(error.names, vec!["nope"]);
        assert!(flags.voice_mode, "known tokens must not be dropped");
    }

    #[test]
    fn known_flags_matches_the_fields() {
        // Every advertised name must parse, and the advertised list must not
        // name a flag that silently does nothing.
        let parsed = FeatureFlags::parse_checked(&KNOWN_FLAGS.join(","))
            .expect("the documented names must all be accepted");
        assert!(parsed.capabilities().bits() > 0);
        // 17 flags: 11 runtime capabilities/experiments plus the six
        // command-availability gates.
        assert_eq!(KNOWN_FLAGS.len(), 17);
    }

    #[test]
    fn capability_set_derives_correctly_from_flags() {
        let flags = FeatureFlags {
            voice_mode: true,
            web_browser_tool: true,
            workflow_scripts: true,
            ..Default::default()
        };

        let caps = flags.capabilities();
        assert!(caps.contains(CapabilitySet::VOICE));
        assert!(caps.contains(CapabilitySet::WEB_BROWSER));
        assert!(caps.contains(CapabilitySet::WORKFLOW));
        assert!(!caps.contains(CapabilitySet::DREAM));
    }

    #[test]
    fn feature_flags_with_experiments_overrides() {
        let flags = FeatureFlags::default();
        let mut experiments = HashMap::new();
        experiments.insert("voice_mode".into(), true);
        experiments.insert("workflow_scripts".into(), true);

        let result = flags.with_experiments(&experiments);
        assert!(result.voice_mode);
        assert!(result.workflow_scripts);
        assert!(!result.coordinator_mode);
    }

    #[test]
    fn feature_flags_all_runtime_flags_from_parse() {
        let flags = FeatureFlags::parse(
            "streaming_tool_execution,tool_use_summaries,fast_mode,coordinator_mode,\
             history_snip,agent_triggers,voice_mode,\
             workflow_scripts,web_browser_tool,team_collaboration,auto_dream",
        );
        assert!(flags.streaming_tool_execution);
        assert!(flags.tool_use_summaries);
        assert!(flags.fast_mode);
        assert!(flags.coordinator_mode);
        assert!(flags.history_snip);
        assert!(flags.agent_triggers);
        assert!(flags.voice_mode);
        assert!(flags.workflow_scripts);
        assert!(flags.web_browser_tool);
        assert!(flags.team_collaboration);
        assert!(flags.auto_dream);
    }

    #[test]
    fn deleted_names_are_no_longer_known() {
        // `context_collapse`, `terminal_panel`, and `kairos` named no product
        // surface, and `bridge_mode`, `daemon` and `buddy` named entry points
        // or a compile-time feature a runtime flag cannot gate. All six were
        // removed from `KNOWN_FLAGS` entirely. Parsing them must now report
        // them as unknown rather than accept a no-op.
        let deleted = [
            "context_collapse",
            "terminal_panel",
            "kairos",
            "bridge_mode",
            "daemon",
            "buddy",
        ];
        let error = FeatureFlags::parse_checked(&deleted.join(","))
            .expect_err("the deleted names are not flags this build knows");
        assert_eq!(error.names, deleted);
        let map = FeatureFlags::default().as_map();
        for name in deleted {
            assert!(!map.contains_key(name), "{name} is still advertised");
            assert!(!FeatureFlags::default().is_enabled(name));
        }
    }

    #[test]
    fn every_inert_flag_is_known_and_parses() {
        // The two lists must agree: an inert name that `KNOWN_FLAGS` does not
        // advertise would be a flag nobody could set, and one that does not
        // parse would be a name the parser rejects while `inert_flags()`
        // reports it.
        for name in INERT_FLAGS {
            assert!(
                KNOWN_FLAGS.contains(name),
                "{name} is inert but not advertised in KNOWN_FLAGS"
            );
            let flags = FeatureFlags::parse_checked(name)
                .unwrap_or_else(|_| panic!("inert flag {name} does not parse"));
            assert!(flags.is_enabled(name), "{name} parsed but is not enabled");
            assert_eq!(flags.inert_flags(), vec![*name]);
        }
    }

    #[test]
    fn flag_registry_read_write() {
        let registry = FlagRegistry::new();
        assert!(!registry.has_capability(CapabilitySet::VOICE));

        registry.set_flag("voice_mode", true);
        assert!(registry.has_capability(CapabilitySet::VOICE));

        let flags = registry.flags();
        assert!(flags.voice_mode);
    }

    #[test]
    fn flag_registry_apply_experiments() {
        let registry = FlagRegistry::new();
        let mut experiments = HashMap::new();
        experiments.insert("voice_mode".into(), true);
        experiments.insert("workflow_scripts".into(), true);

        registry.apply_experiments(&experiments);
        assert!(registry.has_capability(CapabilitySet::VOICE));
        assert!(registry.has_capability(CapabilitySet::WORKFLOW));
    }

    #[test]
    fn percentage_rollout_deterministic() {
        // Same feature + session should always give the same result
        let r1 = percentage_rollout("feature_a", "session_1", 50);
        let r2 = percentage_rollout("feature_a", "session_1", 50);
        assert_eq!(r1, r2);
    }

    #[test]
    fn percentage_rollout_0_percent_always_false() {
        assert!(!percentage_rollout("f", "s", 0));
    }

    #[test]
    fn percentage_rollout_100_percent_always_true() {
        assert!(percentage_rollout("f", "s", 100));
    }

    #[test]
    fn percentage_rollout_different_sessions() {
        // Different sessions may get different assignments
        let _r1 = percentage_rollout("feature", "session_a", 50);
        let _r2 = percentage_rollout("feature", "session_b", 50);
        // We can't assert equality/inequality without knowing the hash,
        // but the test ensures it doesn't panic.
    }

    #[test]
    fn feature_flags_with_project_overrides() {
        let flags = FeatureFlags::default();
        let mut overrides = HashMap::new();
        overrides.insert("voice_mode".into(), true);
        overrides.insert("coordinator_mode".into(), true);

        let result = flags.with_project_overrides(&overrides);
        assert!(result.voice_mode);
        assert!(result.coordinator_mode);
        assert!(!result.workflow_scripts);
    }

    #[test]
    fn load_project_flags_from_file() {
        let tmp = std::env::temp_dir();
        let path = tmp.join("coderex_test_project_flags.json");
        std::fs::write(&path, r#"{"voice_mode":true,"workflow_scripts":true}"#).unwrap();

        let flags = FeatureFlags::load_project_flags(&path);
        assert_eq!(flags.get("voice_mode"), Some(&true));
        assert_eq!(flags.get("workflow_scripts"), Some(&true));
        assert!(!flags.contains_key("coordinator_mode"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_project_flags_missing_file_returns_empty() {
        let flags = FeatureFlags::load_project_flags(std::path::Path::new("/nonexistent/path"));
        assert!(flags.is_empty());
    }

    #[test]
    fn flag_registry_load_project_flags() {
        let tmp = std::env::temp_dir();
        let path = tmp.join("coderex_test_registry_flags.json");
        std::fs::write(&path, r#"{"voice_mode":true,"workflow_scripts":true}"#).unwrap();

        let registry = FlagRegistry::new();
        registry.load_project_flags(&path);
        assert!(registry.has_capability(CapabilitySet::VOICE));
        assert!(registry.has_capability(CapabilitySet::WORKFLOW));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn context_window_experiment_values_parse() {
        let flags = FeatureFlags::parse(
            "context_window_default=64000,\
             context_window=my-gateway:128000,\
             context_window.anthropic/claude-sonnet-4-6=210000",
        );

        assert!(flags.context_window_overrides);
        assert_eq!(flags.context_window_default, Some(64_000));
        assert_eq!(flags.context_windows.get("my-gateway"), Some(&128_000));
        assert_eq!(
            flags.context_windows.get("anthropic/claude-sonnet-4-6"),
            Some(&210_000)
        );

        let overrides = flags.experiment_context_windows();
        assert_eq!(overrides.resolve("my-gateway"), Some(128_000));
        assert_eq!(
            overrides.resolve("claude-sonnet-4-6"),
            Some(210_000),
            "provider-prefixed experiment keys match the bare model name"
        );
        assert_eq!(overrides.resolve("unknown"), Some(64_000));
    }

    #[test]
    fn invalid_context_window_experiment_values_are_ignored() {
        let flags = FeatureFlags::parse(
            "context_window_default=0,\
             context_window=my-gateway:not-a-number,\
             context_window.=128000",
        );

        assert!(!flags.context_window_overrides);
        assert_eq!(flags.context_window_default, None);
        assert!(flags.context_windows.is_empty());
        assert!(flags.experiment_context_windows().is_empty());
    }

    #[test]
    fn context_window_experiment_requires_enabled_flag() {
        let mut flags = FeatureFlags {
            context_window_default: Some(64_000),
            ..Default::default()
        };
        flags.context_windows.insert("my-gateway".into(), 128_000);
        assert!(
            flags.experiment_context_windows().is_empty(),
            "values do not participate while the experiment flag is off"
        );

        flags.context_window_overrides = true;
        let overrides = flags.experiment_context_windows();
        assert_eq!(overrides.resolve("my-gateway"), Some(128_000));
        assert_eq!(overrides.resolve("unknown"), Some(64_000));
    }

    #[test]
    fn command_gate_flags_reach_the_command_context_map() {
        // The slash-command context asks by name. These six used to be neither
        // known flags nor present in the map, so their commands could never
        // run even when a user asked for them.
        let flags =
            FeatureFlags::parse("+ultraplan,+thinkback,+heapdump,+mock_limits,+ctx_viz,+stickers");
        let map = flags.as_map();
        for name in [
            "ultraplan",
            "thinkback",
            "heapdump",
            "mock_limits",
            "ctx_viz",
            "stickers",
        ] {
            assert_eq!(map.get(name), Some(&true), "{name} was not enabled");
            assert!(flags.is_enabled(name), "{name} was not enabled");
        }
        assert_eq!(map.get("voice_mode"), Some(&false));
        assert!(!flags.is_enabled("not_a_flag"));
    }

    #[test]
    fn boolean_context_window_flag_can_be_disabled_by_project_override() {
        let flags = FeatureFlags::parse("context_window_overrides");
        assert!(flags.context_window_overrides);

        let mut overrides = HashMap::new();
        overrides.insert("context_window_overrides".into(), false);
        let with_project = flags.with_project_overrides(&overrides);
        assert!(!with_project.context_window_overrides);
    }

    #[test]
    fn inert_flags_are_empty_when_everything_is_off() {
        assert!(FeatureFlags::default().inert_flags().is_empty());
    }

    #[test]
    fn final_inert_flags_are_empty() {
        // Pin the final list: each of the six names that used to be here was
        // either wired to a product path (streaming_tool_execution,
        // tool_use_summaries, team_collaboration) or deleted because no runtime
        // flag could gate it (bridge_mode, daemon, buddy). A name reappearing
        // here is a regression.
        assert!(
            INERT_FLAGS.is_empty(),
            "unexpected inert flags: {INERT_FLAGS:?}"
        );
        let all = FeatureFlags::parse_checked(&KNOWN_FLAGS.join(","))
            .expect("the documented names must all be accepted");
        assert!(all.inert_flags().is_empty());
    }

    #[test]
    fn inert_flags_excludes_flags_that_have_consumers() {
        // `workflow_scripts` feeds the CapabilitySet, and the six command
        // gates are consulted by their slash commands. None are inert.
        let flags = FeatureFlags::parse(
            "workflow_scripts,ultraplan,thinkback,heapdump,mock_limits,ctx_viz,stickers",
        );
        assert!(
            flags.inert_flags().is_empty(),
            "flags with consumers were reported as inert: {:?}",
            flags.inert_flags()
        );
    }

    #[test]
    fn flags_with_a_product_path_are_not_inert() {
        // `web_browser_tool` gates the WebFetch tool, `agent_triggers` gates
        // the agent-kind hook path, and `auto_dream` gates the auto-dream
        // manager. Each is read by production code, so none may be reported as
        // inert.
        let flags = FeatureFlags::parse("web_browser_tool,agent_triggers,auto_dream");
        assert!(
            flags.inert_flags().is_empty(),
            "effective flags were reported as inert: {:?}",
            flags.inert_flags()
        );
    }

    #[test]
    fn every_known_flag_has_a_product_path_or_a_consumer() {
        // Every known flag enabled: no name may be reported as inert, because
        // every remaining name is either read by production code or gates a
        // command. The list that used to describe the compatibility-only
        // remainder is now empty.
        let flags = FeatureFlags::parse_checked(&KNOWN_FLAGS.join(","))
            .expect("the documented names must all be accepted");
        assert!(
            flags.inert_flags().is_empty(),
            "a known flag is still inert: {:?}",
            flags.inert_flags()
        );
    }

    #[test]
    fn newly_wired_flags_are_no_longer_inert() {
        // `fast_mode`, `coordinator_mode`, and `history_snip` have product
        // paths (the session model, the engine coordinator, and compaction
        // strategy selection); `streaming_tool_execution`, `tool_use_summaries`
        // and `team_collaboration` now reach the engine too. Enabling any of
        // them must not be reported as a compatibility-only no-op.
        let wired = [
            "fast_mode",
            "coordinator_mode",
            "history_snip",
            "streaming_tool_execution",
            "tool_use_summaries",
            "team_collaboration",
        ];
        let flags = FeatureFlags::parse(&wired.join(","));
        for name in wired {
            assert!(flags.is_enabled(name), "{name} did not parse as enabled");
            assert!(
                !INERT_FLAGS.contains(&name),
                "{name} is still listed in INERT_FLAGS"
            );
        }
        assert!(
            flags.inert_flags().is_empty(),
            "wired flags were still reported as inert: {:?}",
            flags.inert_flags()
        );
    }
}
