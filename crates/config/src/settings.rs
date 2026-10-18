//! Settings loading, merging, validation, schema migration, and resolution.

use coderex_core::types::ContextWindowOverrides;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// The current config schema version. Increment when the format changes.
pub const CURRENT_SCHEMA_VERSION: u32 = 4;

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
    /// A settings file exceeded the maximum allowed size and was skipped.
    #[error("settings file {path:?} is {size} bytes, exceeding the {limit} byte limit")]
    TooLarge {
        /// Path to the oversized file.
        path: PathBuf,
        /// Size of the file in bytes.
        size: u64,
        /// The configured maximum size in bytes.
        limit: u64,
    },
    /// A managed-settings property list could not be parsed.
    #[error("plist parse error in {path}: {message}")]
    Plist {
        /// Path to the property list.
        path: PathBuf,
        /// The underlying parser error.
        message: String,
    },
}

/// Maximum size of a single settings file (2 MiB).
///
/// A settings file is loaded on every startup and parsed eagerly; an
/// unbounded file is a cheap way to make the CLI hang or exhaust memory.
/// Files larger than this are skipped whole (see [`load_settings_file`]).
pub const MAX_SETTINGS_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// The set of keys a settings file may contain.
///
/// `env` is accepted for backward compatibility only: releases before the
/// remote-environment split wrote it into `settings.json`. It is deliberately
/// *not* a [`Settings`] field, so serde ignores it and no settings file
/// (including a repository-controlled one) can inject environment variables.
/// Remote environment variables now live in `remote_env.json`.
const KNOWN_SETTINGS_FIELDS: [&str; 16] = [
    "config_version",
    "model",
    "permission_mode",
    "max_budget_usd",
    "telemetry_enabled",
    "telemetry",
    "hooks",
    "mcp_servers",
    "mcpServers",
    "plugins",
    "marketplaces",
    "api_key",
    "context_window",
    "vim_mode",
    "statusline",
    "env",
];

/// Permission modes accepted in settings, most restrictive first.
const VALID_PERMISSION_MODES: [&str; 6] = [
    "default",
    "accept-edits",
    "bypass-permissions",
    "dont-ask",
    "plan",
    "auto",
];

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
            let default_str = entry
                .default
                .as_ref()
                .map_or("null".to_string(), |v| v.to_string());
            let current_str = entry
                .current
                .as_ref()
                .map_or("null".to_string(), |v| v.to_string());
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
    /// OTLP/HTTP logs endpoint (e.g. `http://localhost:4318` or a full
    /// `.../v1/logs` URL). Consumed by the telemetry channel configuration.
    pub otlp_endpoint: Option<String>,
    /// Additional OTLP request headers as `key=value,key2=value2`.
    pub otlp_headers: Option<String>,
    /// Datadog Logs API key. Prefer the environment variable so the key is
    /// not persisted in a settings file.
    pub datadog_api_key: Option<String>,
    /// Datadog site (`datadoghq.com`, `datadoghq.eu`, `us3.datadoghq.com`, …).
    pub datadog_site: Option<String>,
    /// Full Datadog Logs intake URL, overriding the site-derived endpoint.
    pub datadog_endpoint: Option<String>,
}

/// Per-model context-window overrides in settings.
///
/// Shape matches the client-data file (`~/.coderex/context_windows.json`) and
/// the resolver's override type:
///
/// ```json
/// {
///   "context_window": {
///     "default": 128000,
///     "models": { "my-gateway-alias": 200000 }
///   }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContextWindowSettings {
    /// Optional fallback for models without a per-model entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<usize>,
    /// Per-model windows, keyed by model name.
    #[serde(default)]
    pub models: HashMap<String, usize>,
}

impl ContextWindowSettings {
    /// Convert to the core resolver's override type.
    ///
    /// Zero values and blank model names are dropped rather than disabling
    /// compaction: the resolver falls through to the next source instead.
    #[must_use]
    pub fn to_overrides(&self) -> ContextWindowOverrides {
        let models = self
            .models
            .iter()
            .filter(|(model, tokens)| !model.trim().is_empty() && **tokens > 0)
            .map(|(model, tokens)| (model.clone(), *tokens))
            .collect();
        ContextWindowOverrides {
            default: self.default.filter(|tokens| *tokens > 0),
            models,
        }
    }
}

/// Statusline preferences written by `/statusline-setup`.
///
/// Both fields are optional and omitted when unset, so a settings file that
/// only configures, say, the theme round-trips without gaining a `format`
/// key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StatuslineSettings {
    /// Format template for the statusline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Theme name for the statusline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
}

/// A plugin marketplace configured in settings.
///
/// Install and update paths register each entry with the plugin manager before
/// resolving a plugin name, so `coderex plugin install <name>` can find plugins
/// in an organisation's marketplace without an environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceSetting {
    /// Marketplace name, used in status and error messages.
    pub name: String,
    /// HTTPS base URL of the marketplace.
    pub url: String,
}

/// Hook documents grouped by the trust tier of the source they came from.
///
/// [`Settings::hooks`] stays the merged view for compatibility: it is what a
/// caller that just wants "the configured hooks" reads. The session that
/// installs hooks needs the split, though, because a hook's tier is a security
/// property (managed hooks run before plugin hooks, which run before core
/// hooks) and a merged document cannot express it. [`ConfigService::resolve`]
/// therefore also records each layer here.
///
/// The tier is assigned from *where the document was read*, never from a field
/// in the JSON. A repository-controlled settings file cannot put anything into
/// [`managed`](Self::managed), and writing `tier` inside a hook is ignored by
/// the hooks loader.
#[derive(Debug, Clone, Default)]
pub struct HookLayers {
    /// Hooks from built-in defaults, user settings, and project settings.
    pub core: Option<serde_json::Value>,
    /// Hooks supplied by active plugins.
    ///
    /// Filled by the orchestrator from `PluginManager::plugin_hooks()`, which
    /// normalizes the `hooks` value of every active manifest into
    /// `{"hooks":[...]}`. Settings resolution leaves it `None`.
    pub plugin: Option<serde_json::Value>,
    /// Hooks from the administrator-controlled managed-settings carriers.
    pub managed: Option<serde_json::Value>,
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
    /// Hook configurations (the merged, compatibility view).
    pub hooks: Option<serde_json::Value>,
    /// Hook configurations split by the tier of the source they came from.
    ///
    /// Populated by [`ConfigService::resolve`]. Skipped by serde: a settings
    /// file cannot declare its own layers, and the value is omitted when
    /// settings are written back out. `None` means "not resolved" (settings
    /// built by hand); callers then treat [`hooks`](Self::hooks) as the core
    /// layer.
    #[serde(skip)]
    pub hook_layers: Option<HookLayers>,
    /// MCP server configurations.
    ///
    /// Both spellings are accepted: the settings file's snake_case key and the
    /// `mcpServers` spelling used by `.mcp.json` and Claude-compatible
    /// configuration.
    #[serde(alias = "mcpServers")]
    pub mcp_servers: Option<serde_json::Value>,
    /// API key — may be a raw value or a `"keyring://..."` reference.
    pub api_key: Option<String>,
    /// Per-model context-window overrides, with an optional default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<ContextWindowSettings>,
    /// Whether vim keybindings are enabled (written by `/vim`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim_mode: Option<bool>,
    /// Statusline preferences (written by `/statusline-setup`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline: Option<StatuslineSettings>,
}

impl Settings {
    /// Built-in default settings.
    ///
    /// `telemetry_enabled` is deliberately left `None` ("undecided"). Telemetry
    /// is opt-in: a `None` value means the user has not consented, and callers
    /// must treat it as disabled. Setting a `Some(true)` default here would make
    /// the first-run consent gate unreachable and enable telemetry without ever
    /// asking (see `StartupOrchestrator::phase_load_settings`).
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            config_version: CURRENT_SCHEMA_VERSION,
            model: Some("claude-sonnet-4-6".into()),
            permission_mode: Some("default".into()),
            max_budget_usd: None,
            telemetry_enabled: None,
            telemetry: None,
            hooks: None,
            hook_layers: None,
            mcp_servers: None,
            api_key: None,
            context_window: None,
            vim_mode: None,
            statusline: None,
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
            if !VALID_PERMISSION_MODES.contains(&mode.as_str()) {
                return Err(ConfigError::Validation {
                    field: "permission_mode".to_string(),
                    message: format!("must be one of: {}", VALID_PERMISSION_MODES.join(", ")),
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
        if let Some(ref context_window) = self.context_window {
            if context_window.default == Some(0) {
                return Err(ConfigError::Validation {
                    field: "context_window.default".to_string(),
                    message: "context window must be > 0 tokens".to_string(),
                });
            }
            for (model, tokens) in &context_window.models {
                if model.trim().is_empty() {
                    return Err(ConfigError::Validation {
                        field: "context_window.models".to_string(),
                        message: "model name must not be empty".to_string(),
                    });
                }
                if *tokens == 0 {
                    return Err(ConfigError::Validation {
                        field: format!("context_window.models.{model}"),
                        message: "context window must be > 0 tokens".to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Context-window overrides from this settings layer, ready to hand to
    /// [`coderex_core::types::ContextWindowResolver`].
    ///
    /// Zero and blank entries are dropped here as a defensive measure; settings
    /// validation rejects them when the file is loaded.
    #[must_use]
    pub fn context_window_overrides(&self) -> ContextWindowOverrides {
        self.context_window.as_ref().map_or_else(
            ContextWindowOverrides::default,
            ContextWindowSettings::to_overrides,
        )
    }

    /// Merge another Settings into this one.
    ///
    /// Values from `other` override `self` when they are `Some`.
    ///
    /// Every key is winner-take-all: the higher-priority layer replaces the
    /// whole value rather than merging into it (the `telemetry` sub-config is
    /// replaced wholesale, not field-by-field). This is why there is no separate
    /// "winner-only under merge" key table — the property holds for all keys.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            config_version: other.config_version.max(self.config_version),
            model: other.model.clone().or_else(|| self.model.clone()),
            permission_mode: other
                .permission_mode
                .clone()
                .or_else(|| self.permission_mode.clone()),
            max_budget_usd: other.max_budget_usd.or(self.max_budget_usd),
            telemetry_enabled: other.telemetry_enabled.or(self.telemetry_enabled),
            telemetry: other.telemetry.clone().or_else(|| self.telemetry.clone()),
            hooks: other.hooks.clone().or_else(|| self.hooks.clone()),
            // The layer split is recomputed by `resolve` from the actual
            // sources; carrying a stale one through a merge would claim
            // provenance the merged value no longer has.
            hook_layers: None,
            mcp_servers: other
                .mcp_servers
                .clone()
                .or_else(|| self.mcp_servers.clone()),
            api_key: other.api_key.clone().or_else(|| self.api_key.clone()),
            context_window: other
                .context_window
                .clone()
                .or_else(|| self.context_window.clone()),
            vim_mode: other.vim_mode.or(self.vim_mode),
            statusline: other.statusline.clone().or_else(|| self.statusline.clone()),
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
            "model",
            "permission_mode",
            "max_budget_usd",
            "telemetry_enabled",
            "telemetry",
            "hooks",
            "mcp_servers",
            "api_key",
            "context_window",
            "vim_mode",
            "statusline",
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
    if from < 4 {
        // v3 → v4: add context_window overrides (already present via Default)
        settings.config_version = 4;
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

    /// Values that apply to the live session rather than the merged settings.
    ///
    /// `Settings` is the on-disk configuration shape; prompt, fallback-model
    /// and effort choices are session-scoped and are not persisted by this
    /// type, so they are surfaced separately instead of being silently
    /// dropped inside [`Self::apply`].
    #[must_use]
    pub fn session_overrides(&self) -> SessionOverrides {
        SessionOverrides {
            system_prompt: self.system_prompt.clone(),
            append_system_prompt: self.append_system_prompt,
            fallback_model: self.fallback_model.clone(),
            effort: self.effort.clone(),
            allowed_tools: self.allowed_tools.clone(),
        }
    }
}

/// Host-provided plugin configuration read from settings files.
///
/// These keys are not part of [`Settings`] because they configure the plugin
/// host (marketplace registration and `userConfig` values), not the engine's
/// merged configuration. They are still validated as known fields so a typo
/// is reported instead of ignored.
#[derive(Debug, Clone, Default)]
pub struct PluginHostSettings {
    /// The raw `plugins` value, in the plugin manager's settings shape.
    pub plugins: Option<serde_json::Value>,
    /// Configured marketplaces.
    pub marketplaces: Vec<MarketplaceSetting>,
}

/// Session-scoped overrides extracted from [`RuntimeOverrides`].
#[derive(Debug, Clone, Default)]
pub struct SessionOverrides {
    /// Custom system prompt.
    pub system_prompt: Option<String>,
    /// Append to the default system prompt instead of replacing it.
    pub append_system_prompt: bool,
    /// Fallback model.
    pub fallback_model: Option<String>,
    /// Reasoning effort level.
    pub effort: Option<String>,
    /// Allowed tools list.
    pub allowed_tools: Option<Vec<String>>,
}

/// Service for loading and merging settings from all sources.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)] // `*_config_path` is the clearest naming here
pub struct ConfigService {
    user_config_path: PathBuf,
    project_config_path: Option<PathBuf>,
    managed_config_path: Option<PathBuf>,
    managed_plist_path: Option<PathBuf>,
    managed_registry_key: Option<String>,
}

impl ConfigService {
    /// Create a new config service.
    #[must_use]
    pub const fn new(user_config_path: PathBuf, project_config_path: Option<PathBuf>) -> Self {
        Self {
            user_config_path,
            project_config_path,
            managed_config_path: None,
            managed_plist_path: None,
            managed_registry_key: None,
        }
    }

    /// Attach a managed-settings file (the "file" enterprise carrier).
    ///
    /// The file is a [`crate::dirs::ManagedSettings`] document — a `settings`
    /// object plus a SHA-256 `checksum` — and is merged at the highest
    /// priority. When absent, no managed file source is consulted.
    #[must_use]
    pub fn with_managed_settings_path(mut self, path: PathBuf) -> Self {
        self.managed_config_path = Some(path);
        self
    }

    /// Attach a managed-settings property list (the macOS MDM carrier).
    ///
    /// The plist is a dictionary of settings keys, normally installed by an
    /// MDM profile at `/Library/Managed Preferences/<domain>.plist`. It is
    /// merged as a managed source: after the managed JSON file and before the
    /// Windows registry carrier (OS policy wins over the file). No checksum is
    /// verified — the OS protects the location — but the document is still
    /// size-capped, checked for unknown keys, migrated, and validated like any
    /// other settings file.
    #[must_use]
    pub fn with_managed_settings_plist(mut self, path: PathBuf) -> Self {
        self.managed_plist_path = Some(path);
        self
    }

    /// Attach the default macOS managed-preferences plist for CodeRex
    /// (`/Library/Managed Preferences/dev.coderex.plist`).
    #[must_use]
    pub fn with_default_macos_managed_plist(self) -> Self {
        self.with_managed_settings_plist(crate::managed::default_managed_plist_path(
            crate::managed::DEFAULT_MACOS_MANAGED_PLIST_DOMAIN,
        ))
    }

    /// Attach the Windows HKLM registry carrier with an explicit key path.
    ///
    /// `key_path` is relative to `HKEY_LOCAL_MACHINE` (for example
    /// `SOFTWARE\CodeRex`). The carrier is read only when compiled for
    /// Windows; on other platforms it is a no-op.
    #[must_use]
    pub fn with_managed_settings_registry_key(mut self, key_path: impl Into<String>) -> Self {
        self.managed_registry_key = Some(key_path.into());
        self
    }

    /// Attach the default Windows HKLM carrier
    /// (`HKLM\SOFTWARE\CodeRex`).
    #[must_use]
    pub fn with_default_windows_managed_registry(self) -> Self {
        self.with_managed_settings_registry_key(crate::managed::WINDOWS_MANAGED_REGISTRY_KEY)
    }

    /// Attach the platform's default OS managed-settings carrier.
    ///
    /// On macOS this is `/Library/Managed Preferences/dev.coderex.plist`; on
    /// Windows it is `HKLM\SOFTWARE\CodeRex`. Other platforms get no extra
    /// carrier. A carrier already configured explicitly is left untouched, so
    /// an explicit path always wins over the platform default.
    #[must_use]
    pub fn with_platform_managed_settings(mut self) -> Self {
        #[cfg(target_os = "macos")]
        if self.managed_plist_path.is_none() {
            self = self.with_default_macos_managed_plist();
        }
        #[cfg(windows)]
        if self.managed_registry_key.is_none() {
            self = self.with_default_windows_managed_registry();
        }
        self
    }

    /// The managed JSON file carrier path, if one is configured.
    #[must_use]
    pub fn managed_config_path(&self) -> Option<&Path> {
        self.managed_config_path.as_deref()
    }

    /// The managed plist carrier path, if one is configured.
    #[must_use]
    pub fn managed_plist_path(&self) -> Option<&Path> {
        self.managed_plist_path.as_deref()
    }

    /// The managed Windows registry key, if one is configured.
    #[must_use]
    pub fn managed_registry_key(&self) -> Option<&str> {
        self.managed_registry_key.as_deref()
    }

    /// Load managed settings from every configured carrier.
    ///
    /// Returns `Ok(None)` if no managed carrier is configured or none is
    /// present. Carrier precedence, lowest to highest:
    ///
    /// 1. managed JSON file ([`Self::with_managed_settings_path`])
    /// 2. macOS property list ([`Self::with_managed_settings_plist`])
    /// 3. Windows registry ([`Self::with_managed_settings_registry_key`])
    ///
    /// A checksum mismatch on the JSON file is treated as absent (fail closed)
    /// so a tampered managed file cannot relax policy. OS carriers are
    /// protected by the operating system and are skipped with a warning if they
    /// cannot be read or parsed, without discarding a carrier that did load.
    pub fn load_managed_config(&self) -> Result<Option<Settings>, ConfigError> {
        let mut merged = self.load_managed_file_config()?;

        if let Some(path) = &self.managed_plist_path {
            match crate::managed::load_managed_plist(path) {
                Ok(Some(settings)) => {
                    merged = Some(match merged {
                        Some(current) => current.merge(&settings),
                        None => settings,
                    });
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("managed plist at {path:?} not loaded: {e}"),
            }
        }

        if let Some(key_path) = &self.managed_registry_key {
            #[cfg(windows)]
            match crate::managed::load_managed_registry(key_path) {
                Ok(Some(settings)) => {
                    merged = Some(match merged {
                        Some(current) => current.merge(&settings),
                        None => settings,
                    });
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("managed registry key HKLM\\{key_path} not loaded: {e}"),
            }
            #[cfg(not(windows))]
            tracing::debug!(
                "managed registry key HKLM\\{key_path} ignored — this build is not running on Windows"
            );
        }

        Ok(merged)
    }

    /// Load managed settings from the configured JSON file carrier.
    fn load_managed_file_config(&self) -> Result<Option<Settings>, ConfigError> {
        let Some(path) = &self.managed_config_path else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }
        enforce_size_cap(path)?;
        let content = std::fs::read_to_string(path)?;
        let managed: crate::dirs::ManagedSettings =
            serde_json::from_str(&content).map_err(|source| ConfigError::Json {
                path: path.clone(),
                source,
            })?;
        if !managed.verify_checksum() {
            tracing::warn!(
                "managed settings at {path:?} failed checksum verification — ignoring (fail closed)"
            );
            return Ok(None);
        }
        check_unknown_fields(&managed.settings, "managed")?;
        let settings: Settings =
            serde_json::from_value(managed.settings).map_err(|source| ConfigError::Json {
                path: path.clone(),
                source,
            })?;
        Ok(Some(finalize_settings(settings, "managed")?))
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
    /// Merge order: built-in defaults → user config → project config → managed
    /// config. Runtime overrides are applied separately via
    /// `RuntimeOverrides::apply`.
    pub fn resolve(&self) -> Result<Settings, ConfigError> {
        let defaults = Settings::defaults();

        let user = self
            .load_user_config()
            .inspect_err(|e| tracing::debug!("user config not loaded: {e}"))
            .unwrap_or(None)
            .unwrap_or_default();

        let project = self
            .load_project_config()
            .inspect_err(|e| tracing::debug!("project config not loaded: {e}"))
            .unwrap_or(None)
            .unwrap_or_default();

        let managed = self
            .load_managed_config()
            .inspect_err(|e| tracing::debug!("managed config not loaded: {e}"))
            .unwrap_or(None)
            .unwrap_or_default();

        // Keep the pre-managed view so the hooks it contains can be installed
        // at the core tier. Built-in defaults have no hooks today, but they are
        // included so this remains correct if that ever changes.
        let core = defaults.merge(&user).merge(&project);
        let core_hooks = core.hooks.clone();
        let managed_hooks = managed.hooks.clone();

        let mut merged = core.merge(&managed);
        merged.hook_layers = Some(HookLayers {
            core: core_hooks,
            // Filled by the orchestrator from `PluginManager::plugin_hooks()`
            // after plugins load; settings resolution itself has no plugin
            // manifest to read.
            plugin: None,
            managed: managed_hooks,
        });

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

    /// Load plugin host configuration (`plugins`, `marketplaces`) from the
    /// user and project settings files.
    ///
    /// Project values override user values, matching the normal settings
    /// precedence. Files that fail to parse are skipped with a debug log, just
    /// like [`Self::resolve`] does, so a broken plugin block cannot prevent the
    /// session from starting. Managed carriers are not consulted here; their
    /// settings are policy values and plugin hosting is a user/project choice.
    pub fn load_plugin_host_config(&self) -> PluginHostSettings {
        let mut config = PluginHostSettings::default();
        Self::merge_plugin_host_file(&mut config, &self.user_config_path);
        if let Some(path) = &self.project_config_path {
            Self::merge_plugin_host_file(&mut config, path);
        }
        config
    }

    fn merge_plugin_host_file(config: &mut PluginHostSettings, path: &Path) {
        if !path.exists() {
            return;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            tracing::debug!("plugin host settings not read from {path:?}");
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            tracing::debug!("plugin host settings not parsed from {path:?}");
            return;
        };
        if let Some(plugins) = value.get("plugins") {
            config.plugins = Some(plugins.clone());
        }
        if let Some(marketplaces) = value.get("marketplaces") {
            match serde_json::from_value::<Vec<MarketplaceSetting>>(marketplaces.clone()) {
                Ok(parsed) => config.marketplaces = parsed,
                Err(error) => {
                    tracing::warn!("marketplaces in {path:?} ignored: {error}");
                }
            }
        }
    }
}

/// Enforce which settings sources may relax tool-approval behavior.
///
/// Repository-controlled files must not be able to turn on automatic
/// classification or bypass every prompt. Those modes are accepted only from
/// user, flag, or managed policy settings; other sources silently fall back to
/// the next more restrictive configuration layer.
fn enforce_permission_source_policy(settings: &mut Settings, source: &str) {
    let Some(mode) = settings.permission_mode.as_deref() else {
        return;
    };
    let repo_controlled = matches!(source, "project" | "local");
    let security_sensitive = matches!(mode, "auto" | "bypass-permissions");
    if repo_controlled && security_sensitive {
        tracing::warn!(
            "settings permission_mode '{mode}' ignored — only user/flag/managed settings may enable it ({source} settings are repository-controllable)"
        );
        settings.permission_mode = None;
    }
}

/// Enforce which settings sources may turn telemetry on.
///
/// Telemetry is opt-in. Repository-controlled files (project / local settings)
/// are committed to the repository, so they must never be able to enable
/// telemetry for a user who has not consented — they may only *disable* it.
/// An attempt to enable it is dropped, so the value falls back to the next
/// more permissive layer (user / flag / managed). This is the same
/// "repository-controlled files must not relax privacy" reasoning as
/// [`enforce_permission_source_policy`].
fn enforce_telemetry_source_policy(settings: &mut Settings, source: &str) {
    let repo_controlled = matches!(source, "project" | "local");
    if repo_controlled && settings.telemetry_enabled == Some(true) {
        tracing::warn!(
            "settings telemetry_enabled=true ignored — only user/flag/managed settings may enable telemetry ({source} settings are repository-controllable)"
        );
        settings.telemetry_enabled = None;
    }
}

/// Reject a settings file that exceeds [`MAX_SETTINGS_FILE_BYTES`].
///
/// The whole file is skipped rather than truncated: a partially-read settings
/// file is worse than none, because the reader cannot tell which keys were cut.
pub(crate) fn enforce_size_cap(path: &Path) -> Result<(), ConfigError> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_SETTINGS_FILE_BYTES {
        tracing::warn!(
            "settings file {path:?} is {size} bytes (limit {MAX_SETTINGS_FILE_BYTES}) — skipping the whole file"
        );
        return Err(ConfigError::TooLarge {
            path: path.to_path_buf(),
            size,
            limit: MAX_SETTINGS_FILE_BYTES,
        });
    }
    Ok(())
}

/// Reject a parsed settings object that carries a key we do not understand.
pub(crate) fn check_unknown_fields(
    value: &serde_json::Value,
    source: &str,
) -> Result<(), ConfigError> {
    if let Some(obj) = value.as_object() {
        for key in obj.keys() {
            if !KNOWN_SETTINGS_FIELDS.contains(&key.as_str()) {
                tracing::warn!("unknown config field in {source} settings: {key}");
                return Err(ConfigError::UnknownField { field: key.clone() });
            }
        }
    }
    Ok(())
}

/// Migrate, validate, and apply source policies to a freshly-parsed settings value.
///
/// An unrecognized `permission_mode` is dropped (a *restrictive re-read*) rather
/// than rejecting the whole file: the rest of the file still applies, and the
/// effective mode falls back to the next layer — ultimately the built-in
/// `"default"`, the most restrictive mode. Every other invalid value rejects
/// the whole file.
pub(crate) fn finalize_settings(
    mut settings: Settings,
    source: &str,
) -> Result<Settings, ConfigError> {
    migrate_settings(&mut settings, source);

    if let Some(mode) = settings.permission_mode.as_deref() {
        if !VALID_PERMISSION_MODES.contains(&mode) {
            tracing::warn!(
                "invalid permission_mode '{mode}' in {source} settings — ignoring it (falling back to a more restrictive mode)"
            );
            settings.permission_mode = None;
        }
    }

    settings.validate()?;
    enforce_permission_source_policy(&mut settings, source);
    enforce_telemetry_source_policy(&mut settings, source);
    Ok(settings)
}

/// Load and deserialize a settings file, migrating and validating.
///
/// Any error (oversized file, unknown field, invalid JSON, failed validation)
/// rejects the *whole* file: callers see `Err` and fall back to the next layer.
/// A settings file is never partially applied.
fn load_settings_file(path: &Path, source: &str) -> Result<Option<Settings>, ConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    enforce_size_cap(path)?;
    let content = std::fs::read_to_string(path)?;

    // Check for unknown fields by parsing as generic JSON first
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
        check_unknown_fields(&value, source)?;
    }

    let settings: Settings =
        serde_json::from_str(&content).map_err(|source| ConfigError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let settings = finalize_settings(settings, source)?;

    // Write back migrated config
    if let Ok(migrated) = serde_json::to_string_pretty(&settings) {
        let _ = std::fs::write(path, migrated);
    }

    Ok(Some(settings))
}

/// Get the Claude config home directory (~/.coderex or CLAUDE_CONFIG_DIR override).
#[must_use]
pub fn config_home() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR").ok().map_or_else(
        || home::home_dir().unwrap_or_default().join(".coderex"),
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

        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
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
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "max_budget_usd")
        );
    }

    #[test]
    fn settings_validate_invalid_permission_mode_fails() {
        let settings = Settings {
            permission_mode: Some("invalid-mode".into()),
            ..Default::default()
        };
        let err = settings.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "permission_mode")
        );
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
        let service = ConfigService::new(PathBuf::from("/nonexistent/settings.json"), None);
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
    fn runtime_overrides_expose_session_scoped_values() {
        let overrides = RuntimeOverrides {
            system_prompt: Some("be terse".into()),
            append_system_prompt: true,
            fallback_model: Some("claude-haiku-4-5-20251001".into()),
            effort: Some("high".into()),
            allowed_tools: Some(vec!["Read".into()]),
            ..Default::default()
        };
        let session = overrides.session_overrides();
        assert_eq!(session.system_prompt.as_deref(), Some("be terse"));
        assert!(session.append_system_prompt);
        assert_eq!(
            session.fallback_model.as_deref(),
            Some("claude-haiku-4-5-20251001")
        );
        assert_eq!(session.effort.as_deref(), Some("high"));
        assert_eq!(session.allowed_tools, Some(vec!["Read".to_string()]));
    }

    #[test]
    fn append_system_prompt_flag_is_off_by_default() {
        let session = RuntimeOverrides::default().session_overrides();
        assert!(!session.append_system_prompt);
    }

    #[test]
    fn platform_managed_settings_attaches_the_os_carrier() {
        let service = ConfigService::new(std::path::PathBuf::from("/tmp/user.json"), None)
            .with_platform_managed_settings();
        #[cfg(target_os = "macos")]
        assert_eq!(
            service.managed_plist_path(),
            Some(Path::new("/Library/Managed Preferences/dev.coderex.plist"))
        );
        #[cfg(windows)]
        assert_eq!(service.managed_registry_key(), Some(r"SOFTWARE\CodeRex"));
        #[cfg(not(any(target_os = "macos", windows)))]
        assert_eq!(service.managed_plist_path(), None);
    }

    #[test]
    fn explicit_managed_carrier_wins_over_the_platform_default() {
        let explicit = std::path::PathBuf::from("/tmp/explicit.plist");
        let service = ConfigService::new(std::path::PathBuf::from("/tmp/user.json"), None)
            .with_managed_settings_plist(explicit.clone())
            .with_platform_managed_settings();
        assert_eq!(service.managed_plist_path(), Some(explicit.as_path()));
    }

    #[test]
    fn plugin_host_config_reads_user_and_project_values() {
        let tmp = tempfile::tempdir().unwrap();
        let user_path = tmp.path().join("user.json");
        let project_path = tmp.path().join("project.json");
        std::fs::write(
            &user_path,
            r#"{
                "plugins": {"demo": {"options": {"token": "user"}}},
                "marketplaces": [{"name": "user", "url": "https://user.example"}]
            }"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"{
                "plugins": {"demo": {"options": {"token": "project"}}}
            }"#,
        )
        .unwrap();

        let service = ConfigService::new(user_path, Some(project_path));
        let host = service.load_plugin_host_config();
        let plugins = host.plugins.expect("project plugins override user plugins");
        assert_eq!(
            plugins["demo"]["options"]["token"].as_str(),
            Some("project"),
            "project settings must win over user settings"
        );
        assert_eq!(host.marketplaces.len(), 1);
        assert_eq!(host.marketplaces[0].name, "user");
    }

    #[test]
    fn plugin_host_config_skips_malformed_marketplaces() {
        let tmp = tempfile::tempdir().unwrap();
        let user_path = tmp.path().join("user.json");
        std::fs::write(&user_path, r#"{"marketplaces": [{"name": "missing-url"}]}"#).unwrap();
        let service = ConfigService::new(user_path, None);
        assert!(service.load_plugin_host_config().marketplaces.is_empty());
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
            hook_layers: None,
            mcp_servers: None,
            api_key: None,
            context_window: None,
            vim_mode: None,
            statusline: None,
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
        assert!(
            matches!(result, Err(ConfigError::UnknownField { ref field }) if field == "unknown_key")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mcp_servers_accepts_both_spellings() {
        // `.mcp.json` and Claude-compatible settings use `mcpServers`; the
        // settings struct historically only knew `mcp_servers`.
        let camel: Settings =
            serde_json::from_str(r#"{"mcpServers":{"x":{"command":"y"}}}"#).unwrap();
        assert!(camel.mcp_servers.is_some());
        let snake: Settings =
            serde_json::from_str(r#"{"mcp_servers":{"x":{"command":"y"}}}"#).unwrap();
        assert!(snake.mcp_servers.is_some());
    }

    #[test]
    fn vim_mode_and_statusline_parse_and_round_trip() {
        let json = r#"{
            "vim_mode": true,
            "statusline": { "format": "[model] [vim]", "theme": "dark" }
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.vim_mode, Some(true));
        let statusline = settings.statusline.as_ref().unwrap();
        assert_eq!(statusline.format.as_deref(), Some("[model] [vim]"));
        assert_eq!(statusline.theme.as_deref(), Some("dark"));

        let serialized = serde_json::to_string(&settings).unwrap();
        let reparsed: Settings = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reparsed.vim_mode, Some(true));
        assert_eq!(reparsed.statusline, settings.statusline);
    }

    #[test]
    fn statusline_omits_unset_fields_and_absent_section() {
        let settings: Settings =
            serde_json::from_str(r#"{"statusline":{"theme":"dark"}}"#).unwrap();
        let statusline = settings.statusline.as_ref().unwrap();
        assert_eq!(statusline.format, None);
        assert_eq!(statusline.theme.as_deref(), Some("dark"));

        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["statusline"], serde_json::json!({ "theme": "dark" }));
        assert!(value.get("vim_mode").is_none());

        // An unset `statusline` is omitted entirely rather than written as null.
        let empty = serde_json::to_value(Settings::default()).unwrap();
        assert!(empty.get("statusline").is_none());
        assert!(empty.get("vim_mode").is_none());
    }

    /// A file left behind by older `/vim`, `/remote-env`, and
    /// `/statusline-setup` commands must load instead of being rejected as
    /// unknown, which used to fall the whole user file back to defaults.
    #[test]
    fn settings_file_with_vim_env_and_statusline_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "model": "kept-model",
                "vim_mode": true,
                "env": { "FOO": "bar" },
                "statusline": { "format": "[model]", "theme": "dark" }
            }"#,
        )
        .unwrap();

        let resolved = ConfigService::new(path.clone(), None).resolve().unwrap();
        assert_eq!(resolved.model.as_deref(), Some("kept-model"));
        assert_eq!(resolved.vim_mode, Some(true));
        assert_eq!(
            resolved.statusline.as_ref().unwrap().theme.as_deref(),
            Some("dark")
        );

        // `env` is accepted for backward compatibility but is not a Settings
        // field, so the post-load write-back drops it: a repository-controlled
        // settings file can never inject environment variables.
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(!rewritten.contains("\"env\""));
    }

    #[test]
    fn project_settings_cannot_enable_security_sensitive_modes() {
        for mode in ["auto", "bypass-permissions"] {
            let temp = tempfile::tempdir().unwrap();
            let project_path = temp.path().join("project.json");
            std::fs::write(&project_path, format!("{{\"permission_mode\":\"{mode}\"}}")).unwrap();
            let service = ConfigService::new(temp.path().join("user.json"), Some(project_path));
            let settings = service.resolve().unwrap();
            assert_eq!(settings.permission_mode.as_deref(), Some("default"));
        }
    }

    #[test]
    fn user_and_runtime_sources_can_enable_security_sensitive_modes() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        std::fs::write(&user_path, r#"{"permission_mode":"auto"}"#).unwrap();
        let service = ConfigService::new(user_path, None);
        assert_eq!(
            service.resolve().unwrap().permission_mode.as_deref(),
            Some("auto")
        );

        let overridden = RuntimeOverrides {
            permission_mode: Some("bypass-permissions".into()),
            ..Default::default()
        }
        .apply(Settings::defaults());
        assert_eq!(
            overridden.permission_mode.as_deref(),
            Some("bypass-permissions")
        );
    }

    #[test]
    fn resolve_api_key_passthrough() {
        let key = resolve_api_key("my-raw-key");
        assert_eq!(key, "my-raw-key");
    }

    // ── Invalid values: whole-file skip vs. restrictive re-read ──────────

    /// An unrecognized `permission_mode` is dropped (restrictive re-read)
    /// instead of rejecting the whole file — the other keys still apply.
    #[test]
    fn invalid_permission_mode_is_dropped_not_whole_file() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        std::fs::write(
            &user_path,
            r#"{"permission_mode":"god-mode","model":"custom-model"}"#,
        )
        .unwrap();
        let service = ConfigService::new(user_path, None);
        let resolved = service.resolve().unwrap();
        // The invalid mode falls back to the most restrictive default...
        assert_eq!(resolved.permission_mode.as_deref(), Some("default"));
        // ...and the rest of the file still applies.
        assert_eq!(resolved.model, Some("custom-model".into()));
    }

    /// A different invalid value still rejects the whole file.
    #[test]
    fn invalid_model_still_rejects_the_whole_file() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        std::fs::write(
            &user_path,
            r#"{"model":"","permission_mode":"plan","max_budget_usd":7.0}"#,
        )
        .unwrap();
        let service = ConfigService::new(user_path, None);
        let resolved = service.resolve().unwrap();
        // Whole file dropped: neither the valid mode nor the budget survive.
        assert_eq!(resolved.permission_mode.as_deref(), Some("default"));
        assert_eq!(resolved.max_budget_usd, None);
    }

    // ── Telemetry consent / source policy ────────────────────────────────

    /// The default must leave telemetry *undecided*: a `Some(true)` default
    /// made the first-run consent gate (`is_none()`) unreachable and turned
    /// telemetry on without ever asking.
    #[test]
    fn defaults_leave_telemetry_undecided() {
        assert!(Settings::defaults().telemetry_enabled.is_none());
        let service = ConfigService::new(PathBuf::from("/nonexistent/settings.json"), None);
        assert!(service.resolve().unwrap().telemetry_enabled.is_none());
    }

    /// A repository-committed project file must not be able to switch
    /// telemetry on for someone who never consented.
    #[test]
    fn project_settings_cannot_enable_telemetry() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path().join("project.json");
        std::fs::write(&project_path, r#"{"telemetry_enabled":true}"#).unwrap();
        let service = ConfigService::new(temp.path().join("user.json"), Some(project_path));
        assert_eq!(service.resolve().unwrap().telemetry_enabled, None);
    }

    /// The same file may still turn telemetry *off*.
    #[test]
    fn project_settings_can_disable_telemetry() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        std::fs::write(&user_path, r#"{"telemetry_enabled":true}"#).unwrap();
        let project_path = temp.path().join("project.json");
        std::fs::write(&project_path, r#"{"telemetry_enabled":false}"#).unwrap();
        let service = ConfigService::new(user_path, Some(project_path));
        assert_eq!(service.resolve().unwrap().telemetry_enabled, Some(false));
    }

    /// The user's own settings may enable telemetry.
    #[test]
    fn user_settings_can_enable_telemetry() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        std::fs::write(&user_path, r#"{"telemetry_enabled":true}"#).unwrap();
        let service = ConfigService::new(user_path, None);
        assert_eq!(service.resolve().unwrap().telemetry_enabled, Some(true));
    }

    // ── Size cap ─────────────────────────────────────────────────────────

    #[test]
    fn oversized_settings_file_is_skipped_whole() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("user.json");
        let padding = "a".repeat(usize::try_from(MAX_SETTINGS_FILE_BYTES).unwrap() + 1);
        std::fs::write(
            &path,
            format!(r#"{{"model":"custom-model","api_key":"{padding}"}}"#),
        )
        .unwrap();

        assert!(matches!(
            load_settings_file(&path, "user"),
            Err(ConfigError::TooLarge { .. })
        ));

        // The whole file is dropped, so even the valid `model` does not apply.
        let service = ConfigService::new(path, None);
        assert_eq!(
            service.resolve().unwrap().model,
            Some("claude-sonnet-4-6".into())
        );
    }

    #[test]
    fn settings_file_at_the_size_limit_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("user.json");
        // Build a valid file whose byte length is exactly the cap.
        let prefix = r#"{"api_key":""}"#;
        let padding = "a".repeat(usize::try_from(MAX_SETTINGS_FILE_BYTES).unwrap() - prefix.len());
        std::fs::write(&path, format!(r#"{{"api_key":"{padding}"}}"#)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_SETTINGS_FILE_BYTES
        );
        assert!(load_settings_file(&path, "user").is_ok());
    }

    // ── Merge semantics (WINNER_ONLY_UNDER_MERGE) ────────────────────────

    /// Every key is winner-take-all: a higher layer replaces the whole value
    /// instead of deep-merging into it, so no separate "winner-only" table is
    /// needed.
    #[test]
    fn merge_replaces_whole_value_never_deep_merges() {
        let user = Settings {
            telemetry: Some(TelemetrySubConfig {
                endpoint: Some("https://user.example".into()),
                batch_size: Some(5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let project = Settings {
            telemetry: Some(TelemetrySubConfig {
                endpoint: Some("https://project.example".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = user.merge(&project);
        let telemetry = merged.telemetry.unwrap();
        assert_eq!(
            telemetry.endpoint.as_deref(),
            Some("https://project.example")
        );
        // The loser's batch_size does NOT survive — this is whole-value replacement.
        assert_eq!(telemetry.batch_size, None);
    }

    // ── Managed file carrier ─────────────────────────────────────────────

    fn write_managed(path: &Path, settings: serde_json::Value) {
        let managed = crate::dirs::ManagedSettings::new(settings);
        std::fs::write(path, serde_json::to_string(&managed).unwrap()).unwrap();
    }

    #[test]
    fn managed_settings_file_is_applied_last() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        std::fs::write(&user_path, r#"{"model":"user-model"}"#).unwrap();
        let managed_path = temp.path().join("managed_settings.json");
        write_managed(
            &managed_path,
            serde_json::json!({ "model": "managed-model" }),
        );

        let service = ConfigService::new(user_path, None).with_managed_settings_path(managed_path);
        assert_eq!(
            service.resolve().unwrap().model,
            Some("managed-model".into())
        );
    }

    /// Resolution keeps the hook documents apart by the tier of their source:
    /// the merged `hooks` value is winner-take-all (so it only holds the
    /// managed document), while `hook_layers` still carries the core document
    /// that the merged view dropped.
    #[test]
    fn resolve_records_hook_layers_by_source_tier() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        let project_path = temp.path().join("project.json");
        let managed_path = temp.path().join("managed_settings.json");

        let core_hooks = serde_json::json!([
            { "event": "PreToolUse", "command": "core-hook" }
        ]);
        let managed_hooks = serde_json::json!([
            { "event": "PreToolUse", "command": "managed-hook" }
        ]);
        std::fs::write(
            &user_path,
            serde_json::to_string(&serde_json::json!({ "hooks": core_hooks })).unwrap(),
        )
        .unwrap();
        write_managed(&managed_path, serde_json::json!({ "hooks": managed_hooks }));

        let service = ConfigService::new(user_path, Some(project_path))
            .with_managed_settings_path(managed_path);
        let resolved = service.resolve().unwrap();

        let layers = resolved
            .hook_layers
            .as_ref()
            .expect("resolve must record hook layers");
        assert_eq!(layers.core, Some(core_hooks));
        assert_eq!(layers.managed, Some(managed_hooks.clone()));
        assert_eq!(layers.plugin, None);
        // Compatibility view: winner-take-all leaves only managed's document.
        assert_eq!(resolved.hooks, Some(managed_hooks));

        // The split is derived state; it must not leak into a written file.
        let serialized = serde_json::to_value(&resolved).unwrap();
        assert!(serialized.get("hook_layers").is_none());
    }

    #[test]
    fn managed_settings_checksum_mismatch_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let managed_path = temp.path().join("managed_settings.json");
        let managed = crate::dirs::ManagedSettings::new(serde_json::json!({
            "model": "managed-model"
        }));
        let mut tampered = serde_json::to_value(&managed).unwrap();
        tampered["settings"]["model"] = serde_json::json!("tampered-model");
        std::fs::write(&managed_path, serde_json::to_string(&tampered).unwrap()).unwrap();

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_managed_settings_path(managed_path);
        // Fail closed: the tampered value never applies.
        assert_eq!(
            service.resolve().unwrap().model,
            Some("claude-sonnet-4-6".into())
        );
    }

    /// Managed settings are administrator-controlled, not repository-controlled,
    /// so they may enable telemetry (unlike project settings).
    #[test]
    fn managed_settings_may_enable_telemetry() {
        let temp = tempfile::tempdir().unwrap();
        let managed_path = temp.path().join("managed_settings.json");
        write_managed(
            &managed_path,
            serde_json::json!({ "telemetry_enabled": true }),
        );

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_managed_settings_path(managed_path);
        assert_eq!(service.resolve().unwrap().telemetry_enabled, Some(true));
    }

    #[test]
    fn no_managed_path_means_no_managed_source() {
        let temp = tempfile::tempdir().unwrap();
        let service = ConfigService::new(temp.path().join("user.json"), None);
        assert!(service.load_managed_config().unwrap().is_none());
    }

    /// The size cap applies to the managed JSON carrier too: an oversized file
    /// is skipped whole, so not even its valid keys apply.
    #[test]
    fn managed_settings_file_over_the_size_limit_is_skipped_whole() {
        let temp = tempfile::tempdir().unwrap();
        let managed_path = temp.path().join("managed_settings.json");
        let padding = "a".repeat(usize::try_from(MAX_SETTINGS_FILE_BYTES).unwrap());
        write_managed(
            &managed_path,
            serde_json::json!({ "model": "managed-model", "api_key": padding }),
        );
        assert!(
            std::fs::metadata(&managed_path).unwrap().len() > MAX_SETTINGS_FILE_BYTES,
            "the fixture must exceed the cap"
        );

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_managed_settings_path(managed_path);
        assert!(matches!(
            service.load_managed_config(),
            Err(ConfigError::TooLarge { .. })
        ));

        // The whole file is dropped, including the otherwise-valid model.
        assert_eq!(
            service.resolve().unwrap().model,
            Some("claude-sonnet-4-6".into())
        );
    }

    /// A managed JSON carrier of exactly the cap is accepted and applied.
    #[test]
    fn managed_settings_file_at_the_size_limit_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let managed_path = temp.path().join("managed_settings.json");

        // The checksum is fixed-length, so measuring the empty document gives
        // the wrapper overhead to subtract from the cap.
        let overhead = serde_json::to_string(&crate::dirs::ManagedSettings::new(
            serde_json::json!({ "model": "managed-model", "api_key": "" }),
        ))
        .unwrap()
        .len();
        let padding = "a".repeat(usize::try_from(MAX_SETTINGS_FILE_BYTES).unwrap() - overhead);
        write_managed(
            &managed_path,
            serde_json::json!({ "model": "managed-model", "api_key": padding }),
        );
        assert_eq!(
            std::fs::metadata(&managed_path).unwrap().len(),
            MAX_SETTINGS_FILE_BYTES
        );

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_managed_settings_path(managed_path);
        assert_eq!(
            service.resolve().unwrap().model,
            Some("managed-model".into())
        );
    }

    // ── macOS plist carrier ──────────────────────────────────────────────

    #[cfg(target_os = "macos")]
    fn write_managed_plist(path: &Path, entries: &[(&str, plist::Value)]) {
        let mut dict = plist::Dictionary::new();
        for (key, value) in entries {
            dict.insert((*key).into(), value.clone());
        }
        plist::Value::Dictionary(dict).to_file_xml(path).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn managed_plist_is_applied_after_the_managed_file() {
        let temp = tempfile::tempdir().unwrap();
        let managed_file = temp.path().join("managed_settings.json");
        write_managed(
            &managed_file,
            serde_json::json!({ "model": "file-model", "max_budget_usd": 10.0 }),
        );
        let plist_path = temp.path().join("dev.coderex.plist");
        write_managed_plist(
            &plist_path,
            &[
                ("model", plist::Value::String("plist-model".to_string())),
                ("telemetry_enabled", plist::Value::Boolean(true)),
            ],
        );

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_managed_settings_path(managed_file)
            .with_managed_settings_plist(plist_path);
        let resolved = service.resolve().unwrap();

        // The plist wins over the managed file for keys it sets...
        assert_eq!(resolved.model.as_deref(), Some("plist-model"));
        assert_eq!(resolved.telemetry_enabled, Some(true));
        // ...and the file's other keys survive.
        assert_eq!(resolved.max_budget_usd, Some(10.0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unreadable_managed_plist_does_not_discard_the_managed_file() {
        let temp = tempfile::tempdir().unwrap();
        let managed_file = temp.path().join("managed_settings.json");
        write_managed(&managed_file, serde_json::json!({ "model": "file-model" }));
        let plist_path = temp.path().join("broken.plist");
        std::fs::write(&plist_path, b"this is not a plist").unwrap();

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_managed_settings_path(managed_file)
            .with_managed_settings_plist(plist_path);
        // The bad OS carrier is skipped; the valid carrier still applies.
        assert_eq!(
            service.resolve().unwrap().model.as_deref(),
            Some("file-model")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn managed_plist_can_enable_telemetry_and_security_modes() {
        let temp = tempfile::tempdir().unwrap();
        let plist_path = temp.path().join("dev.coderex.plist");
        write_managed_plist(
            &plist_path,
            &[
                ("telemetry_enabled", plist::Value::Boolean(true)),
                (
                    "permission_mode",
                    plist::Value::String("bypass-permissions".to_string()),
                ),
            ],
        );

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_managed_settings_plist(plist_path);
        let resolved = service.resolve().unwrap();
        assert_eq!(resolved.telemetry_enabled, Some(true));
        assert_eq!(
            resolved.permission_mode.as_deref(),
            Some("bypass-permissions")
        );
    }

    #[test]
    fn registry_carrier_is_inert_off_windows() {
        let temp = tempfile::tempdir().unwrap();
        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_default_windows_managed_registry();
        assert!(service.load_managed_config().unwrap().is_none());
        assert_eq!(
            service.resolve().unwrap().model.as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn settings_context_window_parses_and_flows_through_resolve() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("settings.json");
        std::fs::write(
            &user_path,
            r#"{
                "model": "my-gateway-alias",
                "context_window": {
                    "default": 64000,
                    "models": { "my-gateway-alias": 200000 }
                }
            }"#,
        )
        .unwrap();

        let resolved = ConfigService::new(user_path, None).resolve().unwrap();
        let overrides = resolved.context_window_overrides();
        assert_eq!(overrides.resolve("my-gateway-alias"), Some(200_000));
        assert_eq!(overrides.resolve("other-model"), Some(64_000));
    }

    #[test]
    fn settings_context_window_merge_is_winner_take_all() {
        let user = Settings {
            context_window: Some(ContextWindowSettings {
                default: Some(64_000),
                models: HashMap::from([("user-alias".into(), 128_000)]),
            }),
            ..Default::default()
        };
        let project = Settings {
            context_window: Some(ContextWindowSettings {
                default: Some(96_000),
                models: HashMap::from([("project-alias".into(), 256_000)]),
            }),
            ..Default::default()
        };

        let merged = user.merge(&project);
        let overrides = merged.context_window_overrides();
        // The project layer replaces the whole value: the user's per-model
        // entry is gone, so that alias falls back to the project default.
        assert_eq!(overrides.resolve("user-alias"), Some(96_000));
        assert_eq!(overrides.resolve("project-alias"), Some(256_000));
        assert_eq!(overrides.resolve("other"), Some(96_000));
    }

    #[test]
    fn settings_context_window_rejects_zero_and_blank_entries() {
        let zero = Settings {
            context_window: Some(ContextWindowSettings {
                default: Some(0),
                models: HashMap::new(),
            }),
            ..Default::default()
        };
        let err = zero.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "context_window.default")
        );

        let blank = Settings {
            context_window: Some(ContextWindowSettings {
                default: None,
                models: HashMap::from([("  ".into(), 128_000)]),
            }),
            ..Default::default()
        };
        let err = blank.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "context_window.models")
        );
    }

    #[test]
    fn settings_context_window_conversion_drops_invalid_values() {
        let settings = Settings {
            context_window: Some(ContextWindowSettings {
                default: Some(0),
                models: HashMap::from([
                    ("valid".into(), 128_000),
                    ("zero".into(), 0),
                    ("".into(), 64_000),
                ]),
            }),
            ..Default::default()
        };

        let overrides = settings.context_window_overrides();
        assert_eq!(overrides.resolve("valid"), Some(128_000));
        assert_eq!(overrides.resolve("zero"), None);
        assert_eq!(overrides.default, None);
    }

    #[test]
    fn settings_context_window_appears_in_diff() {
        let settings = Settings {
            context_window: Some(ContextWindowSettings {
                default: None,
                models: HashMap::from([("alias".into(), 128_000)]),
            }),
            ..Settings::defaults()
        };
        let diff = settings.diff_from_defaults("user");
        assert!(
            diff.changes
                .iter()
                .any(|entry| entry.key == "context_window")
        );
    }

    #[test]
    fn settings_context_window_is_a_known_field() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"context_window":{"default":64000,"models":{"alias":128000}}}"#,
        )
        .unwrap();

        let loaded = load_settings_file(&path, "user").unwrap().unwrap();
        assert_eq!(
            loaded.context_window.unwrap().models.get("alias"),
            Some(&128_000)
        );
    }
}
