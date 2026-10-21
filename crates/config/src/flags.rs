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
    "bridge_mode",
    "daemon",
    "voice_mode",
    "buddy",
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
/// These names are deliberately kept in [`KNOWN_FLAGS`]: configuration,
/// experiment payloads, and per-project files written by other builds may
/// mention them, and rejecting them would turn a compatibility name into a
/// parse error. Nothing in this build reads them, so [`FeatureFlags`] stores
/// their parsed state but no behavior depends on it. See
/// [`FeatureFlags::inert_flags`].
///
/// A name leaves this list only when a product path reads it. `web_browser_tool`
/// gates the `WebFetch` tool (see `coderex_tools::required_feature_flag`),
/// `agent_triggers` gates the agent-kind hook path, and `auto_dream` gates the
/// auto-dream manager; all three now do something, so they are no longer here.
/// `fast_mode` now resolves the session model through
/// `ModelProfile::Lightweight` when nothing else chose one, `coordinator_mode`
/// installs a `CoordinatorState` on the session engine, and `history_snip`
/// makes compaction prefer the snip strategy as soon as it is warranted; all
/// three are read by the CLI and the engine, so they are no longer here either.
///
/// A name is deleted outright when no code in the workspace could ever be its
/// product surface. `context_collapse`, `terminal_panel`, and `kairos` were
/// removed for that reason: nothing here implements a context-collapse pass, a
/// terminal panel, or a proactive/Kairos mode, so accepting them only promised
/// behavior that did not exist.
///
/// Each name that remains is kept for a specific, checkable reason: the artifact
/// it would gate already exists, but no flag reads it. The reference's behavior
/// and what building the flag would take are recorded per name:
///
/// - `streaming_tool_execution`: the reference lets a beta turn on executing
///   tool calls as they arrive in a streamed response. `core::engine` already
///   does this (`execute_early_tool_call`, driven by
///   `StreamingToolCallAssembler`), unconditionally. Making the flag the switch
///   needs a `QueryEngine` setter that the streaming loop consults.
/// - `tool_use_summaries`: the reference has the model write a one-line summary
///   of each tool use. CodeRex emits only the fixed `ToolCall::activity_summary()`
///   label. Building it takes a `ModelProfile::Lightweight` summarizer call and
///   a surface to render the result.
/// - `bridge_mode`: the reference gates a session-side bridge capability.
///   CodeRex's bridge is the `bridge` subcommand (a separate entry point), so
///   the flag has nothing distinct to gate. Wiring it means either aliasing the
///   subcommand or gating a session-side bridge transport.
/// - `daemon`: the reference runs CodeRex as a background daemon. CodeRex's
///   daemon is the `serve` subcommand, which writes a PID file and runs the
///   server in the foreground; there is no detach path. Wiring the flag means
///   adding one (fork/`setsid`, log redirection) that `serve` does not have.
/// - `buddy`: the reference ships a companion sprite. `crates/ui/src/buddy.rs`
///   is real but sits behind the compile-time `buddy` cargo feature, which this
///   workspace never enables, so a runtime flag cannot reach it.
/// - `team_collaboration`: the reference coordinates an in-process team. The
///   `TeamCreate`/`TeamDelete` tools, `core::team_context::TeamContext`, and the
///   `/team-memory` sync command all exist, but the flag gates none of them and
///   the engine stores a team context without ever reading it.
const INERT_FLAGS: &[&str] = &[
    "streaming_tool_execution",
    "tool_use_summaries",
    "bridge_mode",
    "daemon",
    "buddy",
    "team_collaboration",
];

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
    /// Enable streaming tool execution.
    pub streaming_tool_execution: bool,
    /// Show tool use summaries.
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
    /// Enable bridge mode.
    pub bridge_mode: bool,
    /// Enable daemon mode.
    pub daemon: bool,
    /// Enable voice mode.
    pub voice_mode: bool,
    /// Enable buddy/companion mode.
    pub buddy: bool,
    /// Enable workflow scripts.
    pub workflow_scripts: bool,
    /// Enable web browser tool.
    pub web_browser_tool: bool,
    /// Enable team collaboration.
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
        /// Bridge mode capability.
        const BRIDGE = 1 << 2;
        /// Daemon mode capability.
        const DAEMON = 1 << 3;
        /// Voice mode capability.
        const VOICE = 1 << 4;
        /// Buddy mode capability.
        const BUDDY = 1 << 5;
        /// Workflow scripts capability.
        const WORKFLOW = 1 << 6;
        /// Web browser tool capability.
        const WEB_BROWSER = 1 << 7;
        /// Team collaboration capability.
        const TEAM = 1 << 8;
        /// Dream mode capability.
        const DREAM = 1 << 9;
    }
}

impl FeatureFlags {
    /// Parse feature flags from a comma-separated string.
    ///
    /// Format: `+flag_name` enables, `-flag_name` disables. Bare names default to enable.
    /// Example: `"+streaming_tool_execution,-daemon"`
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
        if self.bridge_mode {
            caps.insert(CapabilitySet::BRIDGE);
        }
        if self.daemon {
            caps.insert(CapabilitySet::DAEMON);
        }
        if self.voice_mode {
            caps.insert(CapabilitySet::VOICE);
        }
        if self.buddy {
            caps.insert(CapabilitySet::BUDDY);
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
    /// See [`INERT_FLAGS`] for why each remaining name is still inert.
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
            "bridge_mode" => self.bridge_mode,
            "daemon" => self.daemon,
            "voice_mode" => self.voice_mode,
            "buddy" => self.buddy,
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
    /// The file format is `{"daemon": true, "bridge_mode": false, ...}`.
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
            "bridge_mode" => self.bridge_mode = enable,
            "daemon" => self.daemon = enable,
            "voice_mode" => self.voice_mode = enable,
            "buddy" => self.buddy = enable,
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
        let flags = FeatureFlags::parse("daemon");
        assert!(flags.daemon);
        assert!(!flags.buddy);
    }

    #[test]
    fn feature_flags_from_parse_enables_and_disables() {
        let flags =
            FeatureFlags::parse("+daemon,+streaming_tool_execution,-streaming_tool_execution");
        assert!(flags.daemon);
        assert!(!flags.streaming_tool_execution);
    }

    #[test]
    fn feature_flags_unknown_flag_no_panic() {
        let flags = FeatureFlags::parse("nonexistent_flag");
        assert!(!flags.daemon);
    }

    #[test]
    fn checked_parse_reports_every_unknown_name() {
        let error = FeatureFlags::parse_checked("+daemon,not_a_flag,-also_missing")
            .expect_err("unknown flags must be reported");
        assert_eq!(error.names, vec!["not_a_flag", "also_missing"]);
        let rendered = error.to_string();
        assert!(rendered.contains("not_a_flag"), "{rendered}");
        assert!(rendered.contains("known flags:"), "{rendered}");
        assert!(rendered.contains("daemon"), "{rendered}");
    }

    #[test]
    fn checked_parse_accepts_plus_minus_and_bare_names() {
        let flags = FeatureFlags::parse_checked("+daemon,-buddy,voice_mode")
            .expect("all three spellings are valid");
        assert!(flags.daemon);
        assert!(!flags.buddy);
        assert!(flags.voice_mode);
    }

    #[test]
    fn apply_comma_list_overlays_existing_flags() {
        let mut flags = FeatureFlags::default();
        flags.daemon = true;
        flags.buddy = true;

        flags
            .apply_comma_list("-daemon,+voice_mode")
            .expect("known flags");
        assert!(!flags.daemon, "a '-' token must disable");
        assert!(flags.buddy, "untouched flags keep their value");
        assert!(flags.voice_mode, "a '+' token must enable");
    }

    #[test]
    fn apply_comma_list_applies_known_tokens_even_with_an_unknown_one() {
        let mut flags = FeatureFlags::default();
        let error = flags
            .apply_comma_list("daemon,nope")
            .expect_err("the typo must be reported");
        assert_eq!(error.names, vec!["nope"]);
        assert!(flags.daemon, "known tokens must not be dropped");
    }

    #[test]
    fn known_flags_matches_the_fields() {
        // Every advertised name must parse, and the advertised list must not
        // name a flag that silently does nothing.
        let parsed = FeatureFlags::parse_checked(&KNOWN_FLAGS.join(","))
            .expect("the documented names must all be accepted");
        assert!(parsed.capabilities().bits() > 0);
        // 20 flags: 14 runtime capabilities/experiments plus the six
        // command-availability gates.
        assert_eq!(KNOWN_FLAGS.len(), 20);
    }

    #[test]
    fn capability_set_derives_correctly_from_flags() {
        let mut flags = FeatureFlags::default();
        flags.daemon = true;
        flags.bridge_mode = true;
        flags.voice_mode = true;

        let caps = flags.capabilities();
        assert!(caps.contains(CapabilitySet::DAEMON));
        assert!(caps.contains(CapabilitySet::BRIDGE));
        assert!(caps.contains(CapabilitySet::VOICE));
        assert!(!caps.contains(CapabilitySet::DREAM));
    }

    #[test]
    fn feature_flags_with_experiments_overrides() {
        let flags = FeatureFlags::default();
        let mut experiments = HashMap::new();
        experiments.insert("daemon".into(), true);
        experiments.insert("buddy".into(), true);

        let result = flags.with_experiments(&experiments);
        assert!(result.daemon);
        assert!(result.buddy);
        assert!(!result.coordinator_mode);
    }

    #[test]
    fn feature_flags_all_runtime_flags_from_parse() {
        let flags = FeatureFlags::parse(
            "streaming_tool_execution,tool_use_summaries,fast_mode,coordinator_mode,\
             history_snip,agent_triggers,bridge_mode,daemon,voice_mode,buddy,\
             workflow_scripts,web_browser_tool,team_collaboration,auto_dream",
        );
        assert!(flags.streaming_tool_execution);
        assert!(flags.tool_use_summaries);
        assert!(flags.fast_mode);
        assert!(flags.coordinator_mode);
        assert!(flags.history_snip);
        assert!(flags.agent_triggers);
        assert!(flags.bridge_mode);
        assert!(flags.daemon);
        assert!(flags.voice_mode);
        assert!(flags.buddy);
        assert!(flags.workflow_scripts);
        assert!(flags.web_browser_tool);
        assert!(flags.team_collaboration);
        assert!(flags.auto_dream);
    }

    #[test]
    fn deleted_names_are_no_longer_known() {
        // `context_collapse`, `terminal_panel`, and `kairos` named no product
        // surface, so they were removed from `KNOWN_FLAGS` entirely. Parsing
        // them must now report them as unknown rather than accept a no-op.
        let error = FeatureFlags::parse_checked("+context_collapse,terminal_panel,kairos")
            .expect_err("the deleted names are not flags this build knows");
        assert_eq!(
            error.names,
            vec!["context_collapse", "terminal_panel", "kairos"]
        );
        let map = FeatureFlags::default().as_map();
        for name in ["context_collapse", "terminal_panel", "kairos"] {
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
        assert!(!registry.has_capability(CapabilitySet::DAEMON));

        registry.set_flag("daemon", true);
        assert!(registry.has_capability(CapabilitySet::DAEMON));

        let flags = registry.flags();
        assert!(flags.daemon);
    }

    #[test]
    fn flag_registry_apply_experiments() {
        let registry = FlagRegistry::new();
        let mut experiments = HashMap::new();
        experiments.insert("daemon".into(), true);
        experiments.insert("buddy".into(), true);

        registry.apply_experiments(&experiments);
        assert!(registry.has_capability(CapabilitySet::DAEMON));
        assert!(registry.has_capability(CapabilitySet::BUDDY));
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
        overrides.insert("daemon".into(), true);
        overrides.insert("coordinator_mode".into(), true);

        let result = flags.with_project_overrides(&overrides);
        assert!(result.daemon);
        assert!(result.coordinator_mode);
        assert!(!result.buddy);
    }

    #[test]
    fn load_project_flags_from_file() {
        let tmp = std::env::temp_dir();
        let path = tmp.join("coderex_test_project_flags.json");
        std::fs::write(&path, r#"{"daemon":true,"buddy":true}"#).unwrap();

        let flags = FeatureFlags::load_project_flags(&path);
        assert_eq!(flags.get("daemon"), Some(&true));
        assert_eq!(flags.get("buddy"), Some(&true));
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
        std::fs::write(&path, r#"{"daemon":true,"voice_mode":true}"#).unwrap();

        let registry = FlagRegistry::new();
        registry.load_project_flags(&path);
        assert!(registry.has_capability(CapabilitySet::DAEMON));
        assert!(registry.has_capability(CapabilitySet::VOICE));

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
    fn inert_flags_reports_exactly_the_enabled_names() {
        let flags = FeatureFlags::parse("+streaming_tool_execution,+daemon,+buddy,-fast_mode");
        assert_eq!(
            flags.inert_flags(),
            vec!["streaming_tool_execution", "daemon", "buddy"]
        );

        // Disabling one of the enabled names removes it from the report.
        let fewer = FeatureFlags::parse("+streaming_tool_execution,+daemon,+buddy,-daemon");
        assert_eq!(
            fewer.inert_flags(),
            vec!["streaming_tool_execution", "buddy"]
        );
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
    fn inert_flags_is_exactly_the_documented_remainder() {
        // Every known flag enabled: the inert report must be exactly the
        // compatibility-only names, in `KNOWN_FLAGS` order, and nothing else.
        let flags = FeatureFlags::parse_checked(&KNOWN_FLAGS.join(","))
            .expect("the documented names must all be accepted");
        assert_eq!(
            flags.inert_flags(),
            vec![
                "streaming_tool_execution",
                "tool_use_summaries",
                "bridge_mode",
                "daemon",
                "buddy",
                "team_collaboration",
            ]
        );
    }

    #[test]
    fn newly_wired_flags_are_no_longer_inert() {
        // `fast_mode`, `coordinator_mode`, and `history_snip` now have product
        // paths (the session model, the engine coordinator, and compaction
        // strategy selection), so enabling any of them must not be reported as
        // a compatibility-only no-op.
        let flags = FeatureFlags::parse("fast_mode,coordinator_mode,history_snip");
        assert!(flags.fast_mode);
        assert!(flags.coordinator_mode);
        assert!(flags.history_snip);
        assert!(
            flags.inert_flags().is_empty(),
            "wired flags were still reported as inert: {:?}",
            flags.inert_flags()
        );
        for name in ["fast_mode", "coordinator_mode", "history_snip"] {
            assert!(
                !INERT_FLAGS.contains(&name),
                "{name} is still listed in INERT_FLAGS"
            );
        }
    }
}
