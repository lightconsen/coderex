//! Feature flags and capability sets.

use bitflags::bitflags;
use coderex_core::types::ContextWindowOverrides;
use std::collections::HashMap;

/// All feature flags supported by CodeRex.
#[derive(Debug, Clone, Default)]
pub struct FeatureFlags {
    /// Enable streaming tool execution.
    pub streaming_tool_execution: bool,
    /// Show tool use summaries.
    pub tool_use_summaries: bool,
    /// Enable fast mode.
    pub fast_mode: bool,
    /// Enable coordinator mode.
    pub coordinator_mode: bool,
    /// Enable history snipping.
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
    /// Enable context collapse.
    pub context_collapse: bool,
    /// Enable terminal panel.
    pub terminal_panel: bool,
    /// Enable Kairos mode.
    pub kairos: bool,
    /// Enable team collaboration.
    pub team_collaboration: bool,
    /// Enable auto dream mode.
    pub auto_dream: bool,
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
        /// Kairos mode capability.
        const KAIROS = 1 << 10;
    }
}

impl FeatureFlags {
    /// Parse feature flags from a comma-separated string.
    ///
    /// Format: `+flag_name` enables, `-flag_name` disables. Bare names default to enable.
    /// Example: `"+streaming_tool_execution,-daemon"`
    ///
    /// Context-window experiment values use assignment tokens:
    /// `context_window_default=64000`, `context_window=my-alias:128000`, or
    /// `context_window.my-alias=128000`. A valid value also enables
    /// `context_window_overrides`.
    #[must_use]
    pub fn parse(input: &str) -> Self {
        let mut flags = Self::default();
        for token in input.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Some((name, value)) = token.split_once('=') {
                flags.apply_value_flag(name.trim(), value.trim());
                continue;
            }
            let (enable, name) = match token.chars().next() {
                Some('+') => (true, &token[1..]),
                Some('-') => (false, &token[1..]),
                _ => (true, token),
            };
            flags.apply_flag(name, enable);
        }
        flags
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
        if self.kairos {
            caps.insert(CapabilitySet::KAIROS);
        }
        caps
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

    fn apply_flag(&mut self, name: &str, enable: bool) {
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
            "context_collapse" => self.context_collapse = enable,
            "terminal_panel" => self.terminal_panel = enable,
            "kairos" => self.kairos = enable,
            "team_collaboration" => self.team_collaboration = enable,
            "auto_dream" => self.auto_dream = enable,
            "context_window_overrides" => self.context_window_overrides = enable,
            _ => tracing::warn!("unknown feature flag: {name}"),
        }
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
    fn feature_flags_all_17_flags_from_parse() {
        let flags = FeatureFlags::parse(
            "streaming_tool_execution,tool_use_summaries,fast_mode,coordinator_mode,\
             history_snip,agent_triggers,bridge_mode,daemon,voice_mode,buddy,\
             workflow_scripts,web_browser_tool,context_collapse,terminal_panel,\
             kairos,team_collaboration,auto_dream",
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
        assert!(flags.context_collapse);
        assert!(flags.terminal_panel);
        assert!(flags.kairos);
        assert!(flags.team_collaboration);
        assert!(flags.auto_dream);
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
    fn boolean_context_window_flag_can_be_disabled_by_project_override() {
        let flags = FeatureFlags::parse("context_window_overrides");
        assert!(flags.context_window_overrides);

        let mut overrides = HashMap::new();
        overrides.insert("context_window_overrides".into(), false);
        let with_project = flags.with_project_overrides(&overrides);
        assert!(!with_project.context_window_overrides);
    }
}
