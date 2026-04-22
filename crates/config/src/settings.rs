//! Settings loading, merging, validation, schema migration, and resolution.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// The current config schema version. Increment when the format changes.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

/// Errors from config loading and validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// I/O error during config loading.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON parse error in a config file.
    #[error("JSON parse error in {path}: {source}")]
    Json {
        /// Path to the config file.
        path: PathBuf,
        /// The underlying serde_json error.
        source: serde_json::Error,
    },
    /// Validation error — constraint violated.
    #[error("validation error: {field}: {message}")]
    Validation {
        /// The field that failed validation.
        field: String,
        /// Description of the violation.
        message: String,
    },
    /// Unknown field in config file.
    #[error("unknown config field: {field}")]
    UnknownField {
        /// The unrecognized field name.
        field: String,
    },
}

/// A single diff entry showing what changed and where.
#[derive(Debug, Clone)]
pub struct ConfigDiffEntry {
    /// The key path (e.g. "model", "max_budget_usd").
    pub key: String,
    /// The default value.
    pub default: Option<serde_json::Value>,
    /// The current value.
    pub current: Option<serde_json::Value>,
    /// Where the current value came from: "user", "project", "env", "override".
    pub source: String,
}

/// The full diff between current settings and defaults.
#[derive(Debug, Clone)]
pub struct ConfigDiff {
    /// List of changed entries.
    pub changes: Vec<ConfigDiffEntry>,
}

impl ConfigDiff {
    /// Check if there are any changes from defaults.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Render the diff as a human-readable string.
    #[must_use]
    pub fn render(&self) -> String {
        if self.changes.is_empty() {
            return "No changes from defaults.".to_string();
        }
        let mut lines = vec![format!("Config diff ({} change(s)):", self.changes.len())];
        for entry in &self.changes {
            let default_str = entry.default.as_ref().map_or("null".to_string(), |v| v.to_string());
            let current_str = entry.current.as_ref().map_or("null".to_string(), |v| v.to_string());
            lines.push(format!(
                "  {}  (default: {}, current: {} [from: {}])",
                entry.key, default_str, current_str, entry.source,
            ));
        }
        lines.join("\n")
    }
}

/// Telemetry-specific sub-configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TelemetrySubConfig {
    /// Endpoint URL for telemetry event uploads.
    pub endpoint: Option<String>,
    /// Explicit crash endpoint override.
    pub crash_endpoint: Option<String>,
    /// Maximum events per batch.
    pub batch_size: Option<usize>,
    /// Upload interval in seconds.
    pub upload_interval_secs: Option<u64>,
    /// Fraction of events to record (0.0–1.0).
    pub sample_rate: Option<f64>,
    /// Optional pinned root certificate PEM for TLS pinning.
    pub pinned_cert_pem: Option<String>,
}

/// Resolved settings after merging all sources.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// The config schema version (used for migrations).
    #[serde(default)]
    pub config_version: u32,
    /// The default model to use.
    pub model: Option<String>,
    /// Permission mode.
    pub permission_mode: Option<String>,
    /// Maximum budget in USD.
    pub max_budget_usd: Option<f64>,
    /// Whether telemetry (crash reports, usage analytics) is enabled.
    pub telemetry_enabled: Option<bool>,
    /// Telemetry sub-configuration (endpoint, batch size, etc.).
    pub telemetry: Option<TelemetrySubConfig>,
    /// Hook configurations.
    pub hooks: Option<serde_json::Value>,
    /// MCP server configurations.
    pub mcp_servers: Option<serde_json::Value>,
    /// API key — may be a raw value or a `"keyring://..."` reference.
    pub api_key: Option<String>,
}

impl Settings {
    /// Built-in default settings.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            config_version: CURRENT_SCHEMA_VERSION,
            model: Some("claude-sonnet-4-6".into()),
            permission_mode: Some("default".into()),
            max_budget_usd: None,
            telemetry_enabled: Some(true),
            telemetry: None,
            hooks: None,
            mcp_servers: None,
            api_key: None,
        }
    }

    /// Validate settings constraints.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(ref model) = self.model {
            if model.trim().is_empty() {
                return Err(ConfigError::Validation {
                    field: "model".to_string(),
                    message: "model must not be empty".to_string(),
                });
            }
        }
        if let Some(budget) = self.max_budget_usd {
            if budget < 0.0 {
                return Err(ConfigError::Validation {
                    field: "max_budget_usd".to_string(),
                    message: "budget must be non-negative".to_string(),
                });
            }
        }
        if let Some(ref mode) = self.permission_mode {
            let valid_modes = ["default", "accept-edits", "plan", "dont-ask"];
            if !valid_modes.contains(&mode.as_str()) {
                return Err(ConfigError::Validation {
                    field: "permission_mode".to_string(),
                    message: format!(
                        "must be one of: {}",
                        valid_modes.join(", ")
                    ),
                });
            }
        }
        if let Some(ref api_key) = self.api_key {
            if api_key.starts_with("keyring://") {
                let item_name = api_key.strip_prefix("keyring://").unwrap();
                if item_name.trim().is_empty() {
                    return Err(ConfigError::Validation {
                        field: "api_key".to_string(),
                        message: "keyring reference must not be empty".to_string(),
                    });
                }
            } else if api_key.trim().is_empty() {
                return Err(ConfigError::Validation {
                    field: "api_key".to_string(),
                    message: "api_key must not be empty".to_string(),
                });
            }
        }
        if let Some(ref telemetry) = self.telemetry {
            if let Some(sample_rate) = telemetry.sample_rate {
                if !(0.0..=1.0).contains(&sample_rate) {
                    return Err(ConfigError::Validation {
                        field: "telemetry.sample_rate".to_string(),
                        message: "sample_rate must be between 0.0 and 1.0".to_string(),
                    });
                }
            }
            if let Some(batch_size) = telemetry.batch_size {
                if batch_size == 0 {
                    return Err(ConfigError::Validation {
                        field: "telemetry.batch_size".to_string(),
                        message: "batch_size must be > 0".to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Merge another Settings into this one.
    ///
    /// Values from `other` override `self` when they are `Some`.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            config_version: other.config_version.max(self.config_version),
            model: other.model.clone().or_else(|| self.model.clone()),
            permission_mode: other.permission_mode.clone().or_else(|| self.permission_mode.clone()),
            max_budget_usd: other.max_budget_usd.or(self.max_budget_usd),
            telemetry_enabled: other.telemetry_enabled.or(self.telemetry_enabled),
            telemetry: other.telemetry.clone().or_else(|| self.telemetry.clone()),
            hooks: other.hooks.clone().or_else(|| self.hooks.clone()),
            mcp_servers: other.mcp_servers.clone().or_else(|| self.mcp_servers.clone()),
            api_key: other.api_key.clone().or_else(|| self.api_key.clone()),
        }
    }

    /// Compute a diff between these settings and the built-in defaults.
    ///
    /// Returns only keys that differ from defaults.
    #[must_use]
    pub fn diff_from_defaults(&self, source: &str) -> ConfigDiff {
        let defaults = Self::defaults();
        let defaults_json = serde_json::to_value(&defaults).unwrap_or_default();
        let current_json = serde_json::to_value(self).unwrap_or_default();

        let keys = [
            "model", "permission_mode", "max_budget_usd",
            "telemetry_enabled", "telemetry", "hooks", "mcp_servers", "api_key",
        ];
        let mut changes = Vec::new();

        for key in keys {
            let default_val = defaults_json.get(key).cloned();
            let current_val = current_json.get(key).cloned();
            if default_val != current_val {
                changes.push(ConfigDiffEntry {
                    key: key.to_string(),
                    default: default_val,
                    current: current_val,
                    source: source.to_string(),
                });
            }
        }

        ConfigDiff { changes }
    }
}

/// Migrate settings from an older schema version to the current version.
pub fn migrate_settings(settings: &mut Settings, source: &str) {
    let from = settings.config_version;
    if from == 0 {
        // v0 → v1: add config_version default
        settings.config_version = 1;
    }
    if from < 2 {
        // v1 → v2: add api_key field (already present via Default)
        settings.config_version = 2;
    }
    if from < 3 {
        // v2 → v3: add telemetry sub-config field (already present via Default)
        settings.config_version = 3;
    }
    if settings.config_version != from {
        tracing::info!(
            "migrated settings from v{from} to v{} (source: {source})",
            CURRENT_SCHEMA_VERSION,
        );
    }
}

/// Runtime overrides applied programmatically (CLI flags, env vars).
#[derive(Debug, Clone, Default)]
pub struct RuntimeOverrides {
    /// Model to use, overriding config.
    pub model: Option<String>,
    /// Permission mode, overriding config.
    pub permission_mode: Option<String>,
    /// Maximum budget in USD, overriding config.
    pub max_budget_usd: Option<f64>,
    /// Telemetry enabled override.
    pub telemetry_enabled: Option<bool>,
    /// API key, overriding config.
    pub api_key: Option<String>,
    /// Custom system prompt override.
    pub system_prompt: Option<String>,
    /// Append to system prompt instead of replacing.
    pub append_system_prompt: bool,
    /// Allowed tools list.
    pub allowed_tools: Option<Vec<String>>,
    /// Fallback model.
    pub fallback_model: Option<String>,
    /// Reasoning effort level.
    pub effort: Option<String>,
}

impl RuntimeOverrides {
    /// Apply runtime overrides to a resolved Settings.
    #[must_use]
    pub fn apply(self, mut settings: Settings) -> Settings {
        if let Some(model) = self.model {
            settings.model = Some(model);
        }
        if let Some(permission_mode) = self.permission_mode {
            settings.permission_mode = Some(permission_mode);
        }
        if let Some(max_budget_usd) = self.max_budget_usd {
            settings.max_budget_usd = Some(max_budget_usd);
        }
        if let Some(telemetry_enabled) = self.telemetry_enabled {
            settings.telemetry_enabled = Some(telemetry_enabled);
        }
        if let Some(api_key) = self.api_key {
            settings.api_key = Some(api_key);
        }
        settings
    }
}

/// Service for loading and merging settings from all sources.
#[derive(Debug, Clone)]
pub struct ConfigService {
    user_config_path: PathBuf,
    project_config_path: Option<PathBuf>,
}

impl ConfigService {
    /// Create a new config service.
    #[must_use]
    pub const fn new(user_config_path: PathBuf, project_config_path: Option<PathBuf>) -> Self {
        Self {
            user_config_path,
            project_config_path,
        }
    }

    /// Load user config from `~/.coderex/settings.json`.
    ///
    /// Returns `Ok(None)` if the file does not exist.
    pub fn load_user_config(&self) -> Result<Option<Settings>, ConfigError> {
        load_settings_file(&self.user_config_path, "user")
    }

    /// Load project config from `.coderex/settings.json`.
    ///
    /// Returns `Ok(None)` if no project config is found.
    pub fn load_project_config(&self) -> Result<Option<Settings>, ConfigError> {
        if let Some(path) = &self.project_config_path {
            return load_settings_file(path, "project");
        }
        Ok(None)
    }

    /// Resolve the final merged settings, migrate if needed, and validate.
    ///
    /// Merge order: built-in defaults → user config → project config.
    /// Runtime overrides are applied separately via `RuntimeOverrides::apply`.
    pub fn resolve(&self) -> Result<Settings, ConfigError> {
        let defaults = Settings::defaults();

        let user = self
            .load_user_config()
            .inspect_err(|e| tracing::debug!("user config not loaded: {e}")).unwrap_or(None)
            .unwrap_or_default();

        let project = self
            .load_project_config()
            .inspect_err(|e| tracing::debug!("project config not loaded: {e}")).unwrap_or(None)
            .unwrap_or_default();

        let mut merged = defaults.merge(&user).merge(&project);

        // Migrate schema if needed
        migrate_settings(&mut merged, "resolved");

        // Validate
        merged.validate()?;

        Ok(merged)
    }

    /// Get a diff between the resolved settings and defaults.
    pub fn diff(&self) -> Result<ConfigDiff, ConfigError> {
        let resolved = self.resolve()?;
        Ok(resolved.diff_from_defaults("merged"))
    }
}

/// Load and deserialize a settings file, migrating and validating.
fn load_settings_file(path: &Path, source: &str) -> Result<Option<Settings>, ConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;

    // Check for unknown fields by parsing as generic JSON first
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
        if let Some(obj) = value.as_object() {
            let known_fields = [
                "config_version", "model", "permission_mode", "max_budget_usd",
                "telemetry_enabled", "telemetry", "hooks", "mcp_servers", "api_key",
            ];
            for key in obj.keys() {
                if !known_fields.contains(&key.as_str()) {
                    tracing::warn!("unknown config field in {path:?}: {key}");
                    return Err(ConfigError::UnknownField { field: key.clone() });
                }
            }
        }
    }

    let mut settings: Settings = serde_json::from_str(&content).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;

    // Migrate and validate
    migrate_settings(&mut settings, source);
    settings.validate()?;

    // Write back migrated config
    if let Ok(migrated) = serde_json::to_string_pretty(&settings) {
        let _ = std::fs::write(path, migrated);
    }

    Ok(Some(settings))
}

/// Get the Claude config home directory (~/.coderex or CLAUDE_CONFIG_DIR override).
#[must_use]
pub fn config_home() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map_or_else(
            || {
                home::home_dir()
                    .unwrap_or_default()
                    .join(".coderex")
            },
            PathBuf::from,
        )
}

/// Resolve an API key reference. If it starts with `keyring://`, look it up
/// in the OS keychain. Otherwise return as-is.
#[must_use]
pub fn resolve_api_key(api_key: &str) -> String {
    if let Some(item) = api_key.strip_prefix("keyring://") {
        #[cfg(feature = "keyring")]
        {
            if let Ok(entry) = keyring::Entry::new("coderex", item) {
                if let Ok(secret) = entry.get_password() {
                    return secret;
                }
            }
            tracing::warn!("keyring lookup failed for '{item}', returning reference as-is");
        }
        #[cfg(not(feature = "keyring"))]
        {
            let _ = item;
            tracing::warn!("keyring feature not compiled in, returning api_key reference as-is");
        }
    }
    api_key.to_string()
}

/// File watcher for hot-reloading config files.
///
/// Watches one or more config files and sends a notification
/// through a watch channel whenever a change is detected.
#[cfg(feature = "hot-reload")]
pub struct ConfigWatcher {
    /// Watch channel that signals when a config file changed.
    changed: tokio::sync::watch::Sender<()>,
    /// The underlying notify watcher.
    _watcher: std::sync::Arc<std::sync::Mutex<notify::RecommendedWatcher>>,
}

#[cfg(feature = "hot-reload")]
impl ConfigWatcher {
    /// Create a new config watcher watching the given file paths.
    ///
    /// Returns an error if the file watcher cannot be initialized.
    pub fn new<P: AsRef<std::path::Path>>(paths: &[P]) -> Result<Self, notify::Error> {
        use notify::{EventKind, RecursiveMode, Watcher};

        let (tx, _rx) = tokio::sync::watch::channel(());
        let tx_clone = tx.clone();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                match event.kind {
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                        let _ = tx_clone.send(());
                        tracing::debug!("config file change detected");
                    }
                    _ => {}
                }
            }
        })?;

        for path in paths {
            let p = path.as_ref();
            if p.exists() {
                watcher.watch(p, RecursiveMode::NonRecursive)?;
                tracing::debug!("watching config file: {:?}", p);
            }
        }

        Ok(Self {
            changed: tx,
            _watcher: std::sync::Arc::new(std::sync::Mutex::new(watcher)),
        })
    }

    /// Subscribe to config change events.
    ///
    /// Returns a receiver that resolves when a config file changes.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_are_correct() {
        let defaults = Settings::defaults();
        assert_eq!(defaults.model, Some("claude-sonnet-4-6".into()));
        assert_eq!(defaults.permission_mode, Some("default".into()));
        assert!(defaults.max_budget_usd.is_none());
        assert!(defaults.hooks.is_none());
        assert!(defaults.mcp_servers.is_none());
        assert_eq!(defaults.config_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn settings_validate_correct() {
        let settings = Settings::defaults();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn settings_validate_empty_model_fails() {
        let settings = Settings {
            model: Some("".into()),
            ..Default::default()
        };
        let err = settings.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation { ref field, .. } if field == "model"));
    }

    #[test]
    fn settings_validate_negative_budget_fails() {
        let settings = Settings {
            max_budget_usd: Some(-5.0),
            ..Default::default()
        };
        let err = settings.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation { ref field, .. } if field == "max_budget_usd"));
    }

    #[test]
    fn settings_validate_invalid_permission_mode_fails() {
        let settings = Settings {
            permission_mode: Some("invalid-mode".into()),
            ..Default::default()
        };
        let err = settings.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Validation { ref field, .. } if field == "permission_mode"));
    }

    #[test]
    fn settings_merge_user_overrides_defaults() {
        let defaults = Settings::defaults();
        let user = Settings {
            model: Some("claude-opus-4-6".into()),
            ..Default::default()
        };
        let merged = defaults.merge(&user);
        assert_eq!(merged.model, Some("claude-opus-4-6".into()));
        assert_eq!(merged.permission_mode, Some("default".into()));
    }

    #[test]
    fn settings_merge_project_overrides_user() {
        let defaults = Settings::defaults();
        let user = Settings {
            model: Some("claude-sonnet-4-6".into()),
            max_budget_usd: Some(10.0),
            ..Default::default()
        };
        let project = Settings {
            max_budget_usd: Some(5.0),
            permission_mode: Some("accept-edits".into()),
            ..Default::default()
        };
        let merged = defaults.merge(&user).merge(&project);
        assert_eq!(merged.model, Some("claude-sonnet-4-6".into()));
        assert_eq!(merged.max_budget_usd, Some(5.0));
        assert_eq!(merged.permission_mode, Some("accept-edits".into()));
    }

    #[test]
    fn settings_resolve_with_missing_files_falls_back_to_defaults() {
        let service = ConfigService::new(
            PathBuf::from("/nonexistent/settings.json"),
            None,
        );
        let resolved = service.resolve().unwrap();
        assert_eq!(resolved.model, Some("claude-sonnet-4-6".into()));
        assert_eq!(resolved.permission_mode, Some("default".into()));
    }

    #[test]
    fn settings_resolve_full_merge_with_temp_files() {
        let tmp = std::env::temp_dir();
        let user_path = tmp.join("coderex_test_user_settings_v2.json");
        let project_path = tmp.join("coderex_test_project_settings_v2.json");

        // Write user config
        let user = Settings {
            model: Some("claude-haiku-4-5-20251001".into()),
            max_budget_usd: Some(20.0),
            ..Default::default()
        };
        std::fs::write(&user_path, serde_json::to_string(&user).unwrap()).unwrap();

        // Write project config
        let project = Settings {
            permission_mode: Some("dont-ask".into()),
            ..Default::default()
        };
        std::fs::write(&project_path, serde_json::to_string(&project).unwrap()).unwrap();

        let service = ConfigService::new(user_path.clone(), Some(project_path.clone()));
        let resolved = service.resolve().unwrap();

        assert_eq!(resolved.model, Some("claude-haiku-4-5-20251001".into()));
        assert_eq!(resolved.permission_mode, Some("dont-ask".into()));
        assert_eq!(resolved.max_budget_usd, Some(20.0));

        // Cleanup
        let _ = std::fs::remove_file(&user_path);
        let _ = std::fs::remove_file(&project_path);
    }

    #[test]
    fn runtime_overrides_apply_on_top_of_resolved() {
        let defaults = Settings::defaults();
        let overrides_struct = RuntimeOverrides {
            model: Some("claude-opus-4-6".into()),
            permission_mode: Some("plan".into()),
            max_budget_usd: Some(50.0),
            ..Default::default()
        };
        let result = overrides_struct.apply(defaults);
        assert_eq!(result.model, Some("claude-opus-4-6".into()));
        assert_eq!(result.permission_mode, Some("plan".into()));
        assert_eq!(result.max_budget_usd, Some(50.0));
    }

    #[test]
    fn diff_from_defaults_detects_changes() {
        let settings = Settings {
            model: Some("custom-model".into()),
            max_budget_usd: Some(99.0),
            ..Settings::defaults()
        };
        let diff = settings.diff_from_defaults("user");
        assert!(!diff.is_empty());
        assert_eq!(diff.changes.len(), 2);
        let keys: Vec<_> = diff.changes.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"model"));
        assert!(keys.contains(&"max_budget_usd"));
    }

    #[test]
    fn diff_from_defaults_empty_when_matching() {
        let settings = Settings::defaults();
        let diff = settings.diff_from_defaults("user");
        assert!(diff.is_empty());
    }

    #[test]
    fn diff_renders_human_readable() {
        let settings = Settings {
            model: Some("custom".into()),
            ..Settings::defaults()
        };
        let diff = settings.diff_from_defaults("user");
        let rendered = diff.render();
        assert!(rendered.contains("model"));
        assert!(rendered.contains("custom"));
    }

    #[test]
    fn migrate_settings_from_v0() {
        let mut settings = Settings {
            config_version: 0,
            model: Some("old-model".into()),
            permission_mode: Some("default".into()),
            max_budget_usd: None,
            telemetry_enabled: Some(true),
            telemetry: None,
            hooks: None,
            mcp_servers: None,
            api_key: None,
        };
        migrate_settings(&mut settings, "test");
        assert_eq!(settings.config_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn unknown_field_rejected() {
        let tmp = std::env::temp_dir();
        let path = tmp.join("coderex_test_unknown_field.json");
        std::fs::write(&path, r#"{"model":"test","unknown_key":"value"}"#).unwrap();
        let result = load_settings_file(&path, "test");
        assert!(matches!(result, Err(ConfigError::UnknownField { ref field }) if field == "unknown_key"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_api_key_passthrough() {
        let key = resolve_api_key("my-raw-key");
        assert_eq!(key, "my-raw-key");
    }
}
