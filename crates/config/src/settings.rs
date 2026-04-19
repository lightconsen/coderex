//! Settings loading, merging, and resolution.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors from config loading.
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
}

/// Resolved settings after merging all sources.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// The default model to use.
    pub model: Option<String>,
    /// Permission mode.
    pub permission_mode: Option<String>,
    /// Maximum budget in USD.
    pub max_budget_usd: Option<f64>,
    /// Whether telemetry (crash reports, usage analytics) is enabled.
    pub telemetry_enabled: Option<bool>,
    /// Hook configurations.
    pub hooks: Option<serde_json::Value>,
    /// MCP server configurations.
    pub mcp_servers: Option<serde_json::Value>,
}

impl Settings {
    /// Built-in default settings.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            model: Some("claude-sonnet-4-6".into()),
            permission_mode: Some("default".into()),
            max_budget_usd: None,
            telemetry_enabled: Some(true),
            hooks: None,
            mcp_servers: None,
        }
    }

    /// Merge another Settings into this one.
    ///
    /// Values from `other` override `self` when they are `Some`.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            model: other.model.clone().or_else(|| self.model.clone()),
            permission_mode: other.permission_mode.clone().or_else(|| self.permission_mode.clone()),
            max_budget_usd: other.max_budget_usd.or(self.max_budget_usd),
            telemetry_enabled: other.telemetry_enabled.or(self.telemetry_enabled),
            hooks: other.hooks.clone().or_else(|| self.hooks.clone()),
            mcp_servers: other.mcp_servers.clone().or_else(|| self.mcp_servers.clone()),
        }
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

    /// Load user config from `~/.claude/settings.json`.
    ///
    /// Returns `Ok(None)` if the file does not exist.
    pub fn load_user_config(&self) -> Result<Option<Settings>, ConfigError> {
        load_settings_file(&self.user_config_path)
    }

    /// Load project config from `.claude/settings.json` in the current directory or nearest ancestor.
    ///
    /// Returns `Ok(None)` if no project config is found.
    pub fn load_project_config(&self) -> Result<Option<Settings>, ConfigError> {
        if let Some(path) = &self.project_config_path {
            return load_settings_file(path);
        }
        Ok(None)
    }

    /// Resolve the final merged settings.
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

        let merged = defaults.merge(&user).merge(&project);
        Ok(merged)
    }
}

/// Load and deserialize a settings file from the given path.
fn load_settings_file(path: &Path) -> Result<Option<Settings>, ConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)?;
    let settings: Settings = serde_json::from_str(&content).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(settings))
}

/// Get the Claude config home directory (~/.claude or CLAUDE_CONFIG_DIR override).
#[must_use]
pub fn config_home() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map_or_else(
            || {
                home::home_dir()
                    .unwrap_or_default()
                    .join(".claude")
            },
            PathBuf::from,
        )
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
        let user_path = tmp.join("coderex_test_user_settings.json");
        let project_path = tmp.join("coderex_test_project_settings.json");

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
        };
        let result = overrides_struct.apply(defaults);
        assert_eq!(result.model, Some("claude-opus-4-6".into()));
        assert_eq!(result.permission_mode, Some("plan".into()));
        assert_eq!(result.max_budget_usd, Some(50.0));
    }
}
