//! Settings loading, merging, validation, schema migration, and resolution.
use coderex_core::permissions::{PermissionAction, PermissionRuleSpec};

use coderex_core::types::ContextWindowOverrides;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
/// `env` is a real field, but one with a source restriction: only the user
/// and managed layers may set it (the project layers are stripped in
/// [`Settings::resolve`]), so a repository-controlled file still cannot
/// inject environment variables into a session.
const KNOWN_SETTINGS_FIELDS: [&str; 19] = [
    "config_version",
    "model",
    "permission_mode",
    "permissions",
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
    "prompt_caching",
    "env",
    "provider",
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
                .map_or_else(|| "null".to_string(), std::string::ToString::to_string);
            // `--setting-sources` is often pasted into an issue; never print
            // a credential or an environment map that may contain one.
            let current_str = if matches!(entry.key.as_str(), "api_key" | "env") {
                "<redacted>".to_string()
            } else {
                entry
                    .current
                    .as_ref()
                    .map_or_else(|| "null".to_string(), std::string::ToString::to_string)
            };
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
    /// Enable the strict (HIPAA-style) redaction mode.
    ///
    /// Equivalent to setting `CODEREX_HIPAA=1`. Only ever *tightens* what is
    /// sent, so a project settings file enabling it cannot leak more than the
    /// user's own choice would; disabling it here does not override the
    /// environment variable.
    #[serde(default)]
    pub strict: Option<bool>,
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

/// Endpoint overrides for the Anthropic-protocol provider.
///
/// These live in settings rather than in an environment wrapper so a provider
/// (an Anthropic-compatible gateway, a regional endpoint) can be configured
/// once for every CodeRex invocation. The block is only honoured from the
/// **user** and **managed** layers: a repository-controlled
/// `.coderex/settings.json` must not be able to point the session's API key at
/// an endpoint it controls (see [`Settings::resolve`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderSettings {
    /// Base URL for the Anthropic-protocol endpoint. `None` uses the
    /// provider's default (Anthropic's API, or the cloud deployment's URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// HTTP request timeout in seconds. `None` keeps the transport default
    /// (120 s). Raise it for providers that take a long time to answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_secs: Option<u64>,
}

/// Claude-compatible permission rules from a settings layer.
///
/// Rules are raw Claude-style strings (`Bash(git diff:*)`, `Read(./.env)`).
/// They are parsed and validated when the file loads; compilation against the
/// session working directory happens when they are installed into a checker.
///
/// Unlike every other settings key, rules are **unioned** across layers rather
/// than replaced: a lower layer's allow cannot be deleted by a higher one, but
/// a higher layer's deny/ask still wins by precedence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionsSettings {
    /// Rules that allow a matching call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Rules that deny a matching call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Rules that force a prompt for a matching call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
}

impl PermissionsSettings {
    /// Whether this layer carries no rules.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.ask.is_empty()
    }

    /// Append `other`'s rules after this layer's, stable-deduplicated.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        merge_rules(&mut merged.allow, &other.allow);
        merge_rules(&mut merged.deny, &other.deny);
        merge_rules(&mut merged.ask, &other.ask);
        merged
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (field, action, rules) in [
            ("allow", PermissionAction::Allow, &self.allow),
            ("deny", PermissionAction::Deny, &self.deny),
            ("ask", PermissionAction::Ask, &self.ask),
        ] {
            for (index, raw) in rules.iter().enumerate() {
                PermissionRuleSpec::parse(raw, action).map_err(|error| {
                    ConfigError::Validation {
                        field: format!("permissions.{field}[{index}]"),
                        message: format!("invalid permission rule {raw:?}: {error}"),
                    }
                })?;
            }
        }
        Ok(())
    }
}

fn merge_rules(target: &mut Vec<String>, source: &[String]) {
    for rule in source {
        if !target.contains(rule) {
            target.push(rule.clone());
        }
    }
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
    /// Claude-compatible allow/deny/ask rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsSettings>,
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
    /// Provider endpoint overrides (base URL and request timeout).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderSettings>,
    /// Environment variables applied to the session at startup.
    ///
    /// Claude-compatible. Honoured only from the user and managed layers:
    /// project settings are stripped during resolution, so a cloned
    /// repository cannot inject variables such as `ANTHROPIC_BASE_URL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Per-model context-window overrides, with an optional default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<ContextWindowSettings>,
    /// Whether vim keybindings are enabled (written by `/vim`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim_mode: Option<bool>,
    /// Statusline preferences (written by `/statusline-setup`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline: Option<StatuslineSettings>,
    /// Whether Anthropic-protocol requests carry prompt-cache breakpoints.
    ///
    /// `None` means the default (enabled). `CODEREX_PROMPT_CACHING=0`
    /// overrides this setting and disables the markers. OpenAI-compatible
    /// providers ignore it: they cache automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_caching: Option<bool>,
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
            permissions: None,
            max_budget_usd: None,
            telemetry_enabled: None,
            telemetry: None,
            hooks: None,
            hook_layers: None,
            mcp_servers: None,
            api_key: None,
            provider: None,
            env: None,
            context_window: None,
            vim_mode: None,
            statusline: None,
            prompt_caching: None,
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
        if let Some(ref provider) = self.provider {
            if let Some(ref base_url) = provider.base_url {
                let base_url = base_url.trim();
                if base_url.is_empty() {
                    return Err(ConfigError::Validation {
                        field: "provider.base_url".to_string(),
                        message: "base_url must not be empty".to_string(),
                    });
                }
                if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
                    return Err(ConfigError::Validation {
                        field: "provider.base_url".to_string(),
                        message: "base_url must start with http:// or https://".to_string(),
                    });
                }
            }
            if provider.request_timeout_secs == Some(0) {
                return Err(ConfigError::Validation {
                    field: "provider.request_timeout_secs".to_string(),
                    message: "request_timeout_secs must be > 0".to_string(),
                });
            }
        }
        if let Some(ref env) = self.env {
            for key in env.keys() {
                if key.trim().is_empty() {
                    return Err(ConfigError::Validation {
                        field: "env".to_string(),
                        message: "environment variable names must not be empty".to_string(),
                    });
                }
                if key.contains('=') || key.contains('\0') {
                    return Err(ConfigError::Validation {
                        field: format!("env.{key}"),
                        message: "environment variable names must not contain '=' or NUL"
                            .to_string(),
                    });
                }
            }
            if env.values().any(|value| value.contains('\0')) {
                return Err(ConfigError::Validation {
                    field: "env".to_string(),
                    message: "environment variable values must not contain NUL".to_string(),
                });
            }
        }
        if let Some(ref permissions) = self.permissions {
            permissions.validate()?;
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
    /// Every key is winner-take-all except `permissions`: the higher-priority
    /// layer replaces the whole value rather than merging into it (the
    /// `telemetry` sub-config is replaced wholesale, not field-by-field).
    /// Permission rules are the exception because the layers are additive: a
    /// user allow must survive a project layer that only adds a deny.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            config_version: other.config_version.max(self.config_version),
            model: other.model.clone().or_else(|| self.model.clone()),
            permission_mode: other
                .permission_mode
                .clone()
                .or_else(|| self.permission_mode.clone()),
            permissions: match (&self.permissions, &other.permissions) {
                (None, None) => None,
                (Some(lower), None) => Some(lower.clone()),
                (None, Some(higher)) => Some(higher.clone()),
                (Some(lower), Some(higher)) => Some(lower.union(higher)),
            },
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
            provider: other.provider.clone().or_else(|| self.provider.clone()),
            env: other.env.clone().or_else(|| self.env.clone()),
            context_window: other
                .context_window
                .clone()
                .or_else(|| self.context_window.clone()),
            vim_mode: other.vim_mode.or(self.vim_mode),
            statusline: other.statusline.clone().or_else(|| self.statusline.clone()),
            prompt_caching: other.prompt_caching.or(self.prompt_caching),
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
            "permissions",
            "max_budget_usd",
            "telemetry_enabled",
            "telemetry",
            "hooks",
            "mcp_servers",
            "api_key",
            "provider",
            "env",
            "context_window",
            "vim_mode",
            "statusline",
            "prompt_caching",
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
    /// Text appended to the default system prompt.
    ///
    /// Independent of [`system_prompt`](Self::system_prompt): the replacement
    /// prompt supplies the base, and this text is still appended to it.
    pub append_system_prompt: Option<String>,
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
            append_system_prompt: self.append_system_prompt.clone(),
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
    /// Text appended to the system prompt.
    pub append_system_prompt: Option<String>,
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
    local_config_path: Option<PathBuf>,
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
            local_config_path: None,
            managed_config_path: None,
            managed_plist_path: None,
            managed_registry_key: None,
        }
    }

    /// Attach a project-local settings file (`.coderex/settings.local.json`).
    ///
    /// The local file is merged after the project file and before managed
    /// policy, so it can override project settings without escaping the
    /// repository-controlled source policy: like `project`, a `local` file
    /// cannot enable `auto`/`bypass-permissions` or telemetry.
    #[must_use]
    pub fn with_local_config_path(mut self, path: PathBuf) -> Self {
        self.local_config_path = Some(path);
        self
    }

    /// The project-local settings file path, if one is configured.
    #[must_use]
    pub fn local_config_path(&self) -> Option<&Path> {
        self.local_config_path.as_deref()
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
    // On platforms with no managed-settings carrier this is just `self`, so
    // `mut` and the const-able body have nothing to do.
    #[allow(unused_mut, clippy::missing_const_for_fn)]
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

    /// Load project-local config from `.coderex/settings.local.json`.
    ///
    /// Returns `Ok(None)` if no local config path is configured or the file
    /// does not exist.
    pub fn load_local_config(&self) -> Result<Option<Settings>, ConfigError> {
        if let Some(path) = &self.local_config_path {
            return load_settings_file(path, "local");
        }
        Ok(None)
    }

    /// Resolve the final merged settings, migrate if needed, and validate.
    ///
    /// Merge order: built-in defaults → user config → project config → local
    /// config → managed config. Runtime overrides are applied separately via
    /// `RuntimeOverrides::apply`.
    pub fn resolve(&self) -> Result<Settings, ConfigError> {
        let defaults = Settings::defaults();

        let user = self
            .load_user_config()
            .inspect_err(|e| tracing::debug!("user config not loaded: {e}"))
            .unwrap_or(None)
            .unwrap_or_default();

        let mut project = self
            .load_project_config()
            .inspect_err(|e| tracing::debug!("project config not loaded: {e}"))
            .unwrap_or(None)
            .unwrap_or_default();

        let mut local = self
            .load_local_config()
            .inspect_err(|e| tracing::debug!("local config not loaded: {e}"))
            .unwrap_or(None)
            .unwrap_or_default();

        // Provider settings name the endpoint that will receive the session's
        // API key, and `env` can inject any variable the session reads. A
        // repository-controlled settings file must control neither, so only
        // the user and managed layers contribute; the project and
        // project-local layers are ignored for these blocks.
        project.provider = None;
        project.env = None;
        local.provider = None;
        local.env = None;

        let managed = self
            .load_managed_config()
            .inspect_err(|e| tracing::debug!("managed config not loaded: {e}"))
            .unwrap_or(None)
            .unwrap_or_default();

        // Keep the pre-managed view so the hooks it contains can be installed
        // at the core tier. Built-in defaults have no hooks today, but they are
        // included so this remains correct if that ever changes.
        let core = defaults.merge(&user).merge(&project).merge(&local);
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
    /// user, project, and local settings files.
    ///
    /// Precedence matches [`Self::resolve`]: local values override project
    /// values, which override user values. Files that fail to parse are
    /// skipped with a debug log, just like [`Self::resolve`] does, so a broken
    /// plugin block cannot prevent the session from starting. Managed carriers
    /// are not consulted here; their settings are policy values and plugin
    /// hosting is a user/project choice.
    pub fn load_plugin_host_config(&self) -> PluginHostSettings {
        let mut config = PluginHostSettings::default();
        Self::merge_plugin_host_file(&mut config, &self.user_config_path);
        if let Some(path) = &self.project_config_path {
            Self::merge_plugin_host_file(&mut config, path);
        }
        if let Some(path) = &self.local_config_path {
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

    let original: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
    let original_version = original
        .get("config_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let settings: Settings =
        serde_json::from_str(&content).map_err(|source| ConfigError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let settings = finalize_settings(settings, source)?;

    // Write back only an actual migration. A rewrite on every load used to
    // serialize the modelled `Settings` back over the file, silently deleting
    // keys the struct does not carry (`plugins`, `marketplaces`) — which is
    // exactly where plugin `userConfig` lives. Merge the migrated fields onto
    // the original document so unmodelled but allowed keys survive too.
    if u32::try_from(original_version).unwrap_or(u32::MAX) != settings.config_version
        && let Ok(migrated) = serde_json::to_value(&settings)
        && let Some(mut document) = original.as_object().cloned()
        && let Some(fields) = migrated.as_object()
    {
        for (key, value) in fields {
            document.insert(key.clone(), value.clone());
        }
        if let Ok(text) = serde_json::to_string_pretty(&document) {
            let _ = std::fs::write(path, text);
        }
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
/// Watches one or more config files and sends the paths that changed through
/// a watch channel whenever a change is detected. The paths come from the
/// `notify` event, so a consumer can say which file changed (and a hook can
/// name it) instead of only that something did.
#[cfg(feature = "hot-reload")]
pub struct ConfigWatcher {
    /// Watch channel that signals when a config file changed.
    ///
    /// The payload is the changed paths as `notify` reported them. Keeping
    /// the watcher alive keeps this sender alive; dropping it closes the
    /// channel.
    changed: tokio::sync::watch::Sender<Vec<String>>,
    /// The underlying notify watcher.
    _watcher: std::sync::Arc<std::sync::Mutex<notify::RecommendedWatcher>>,
}

#[cfg(feature = "hot-reload")]
impl std::fmt::Debug for ConfigWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The `notify` watcher has no useful representation; the type and its
        // channel are the parts a caller can reason about.
        f.debug_struct("ConfigWatcher").finish_non_exhaustive()
    }
}

#[cfg(feature = "hot-reload")]
impl ConfigWatcher {
    /// Create a new config watcher watching the given file paths.
    ///
    /// Returns an error if the file watcher cannot be initialized.
    pub fn new<P: AsRef<std::path::Path>>(paths: &[P]) -> Result<Self, notify::Error> {
        use notify::{EventKind, RecursiveMode, Watcher};

        let (tx, _rx) = tokio::sync::watch::channel(Vec::new());
        let tx_clone = tx.clone();

        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                            let paths: Vec<String> = event
                                .paths
                                .iter()
                                .map(|path| path.to_string_lossy().into_owned())
                                .collect();
                            tracing::debug!(?paths, "config file change detected");
                            let _ = tx_clone.send(paths);
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
    /// Returns a receiver that resolves when a config file changes; its value
    /// is the list of paths from the `notify` event.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Vec<String>> {
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
            append_system_prompt: Some("add this".into()),
            fallback_model: Some("claude-haiku-4-5-20251001".into()),
            effort: Some("high".into()),
            allowed_tools: Some(vec!["Read".into()]),
            ..Default::default()
        };
        let session = overrides.session_overrides();
        assert_eq!(session.system_prompt.as_deref(), Some("be terse"));
        assert_eq!(session.append_system_prompt.as_deref(), Some("add this"));
        assert_eq!(
            session.fallback_model.as_deref(),
            Some("claude-haiku-4-5-20251001")
        );
        assert_eq!(session.effort.as_deref(), Some("high"));
        assert_eq!(session.allowed_tools, Some(vec!["Read".to_string()]));
    }

    #[test]
    fn append_system_prompt_is_absent_by_default_and_carries_its_text() {
        let session = RuntimeOverrides::default().session_overrides();
        assert!(session.append_system_prompt.is_none());

        // The replacement prompt and the additive text are independent, so
        // both survive `session_overrides`.
        let session = RuntimeOverrides {
            system_prompt: Some("replace".into()),
            append_system_prompt: Some("append".into()),
            ..Default::default()
        }
        .session_overrides();
        assert_eq!(session.system_prompt.as_deref(), Some("replace"));
        assert_eq!(session.append_system_prompt.as_deref(), Some("append"));
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
    fn diff_renders_secrets_redacted() {
        let settings = Settings {
            api_key: Some("sk-do-not-print".into()),
            env: Some(BTreeMap::from([(
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "also-secret".to_string(),
            )])),
            ..Settings::defaults()
        };
        let rendered = settings.diff_from_defaults("user").render();
        assert!(!rendered.contains("sk-do-not-print"), "{rendered}");
        assert!(!rendered.contains("also-secret"), "{rendered}");
        assert_eq!(rendered.matches("<redacted>").count(), 2, "{rendered}");
    }

    #[test]
    fn migrate_settings_from_v0() {
        let mut settings = Settings {
            config_version: 0,
            model: Some("old-model".into()),
            permission_mode: Some("default".into()),
            permissions: None,
            max_budget_usd: None,
            telemetry_enabled: Some(true),
            telemetry: None,
            hooks: None,
            hook_layers: None,
            mcp_servers: None,
            api_key: None,
            provider: None,
            env: None,
            context_window: None,
            vim_mode: None,
            statusline: None,
            prompt_caching: None,
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

        // The user layer's `env` block is kept: it is applied to the session
        // at startup. Project layers are stripped during resolution instead.
        assert_eq!(
            resolved
                .env
                .as_ref()
                .and_then(|env| env.get("FOO"))
                .map(String::as_str),
            Some("bar")
        );
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("\"env\""));
    }

    /// A provider endpoint and timeout are ordinary user settings, so neither
    /// needs a per-provider environment wrapper.
    #[test]
    fn provider_endpoint_settings_are_parsed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "model": "deepseek-flash",
                "api_key": "sk-test",
                "provider": {
                    "base_url": "https://api.deepseek.com/anthropic",
                    "request_timeout_secs": 3000
                }
            }"#,
        )
        .unwrap();

        let resolved = ConfigService::new(path, None).resolve().unwrap();
        let provider = resolved.provider.as_ref().unwrap();
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(provider.request_timeout_secs, Some(3000));
    }

    /// The provider block names the endpoint that receives the API key, so a
    /// repository-controlled settings file must not be able to override it.
    #[test]
    fn project_settings_cannot_redirect_the_provider_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        let project_path = temp.path().join("project.json");
        std::fs::write(
            &user_path,
            r#"{"provider":{"base_url":"https://user.example","request_timeout_secs":900}}"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"{"provider":{"base_url":"https://attacker.example","request_timeout_secs":1}}"#,
        )
        .unwrap();

        let resolved = ConfigService::new(user_path, Some(project_path))
            .resolve()
            .unwrap();
        let provider = resolved.provider.as_ref().unwrap();
        assert_eq!(provider.base_url.as_deref(), Some("https://user.example"));
        assert_eq!(provider.request_timeout_secs, Some(900));
    }

    /// A project-only provider block is dropped whole: it cannot even fill a
    /// gap the user left unset.
    #[test]
    fn a_project_only_provider_block_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let project_path = temp.path().join("project.json");
        std::fs::write(
            &project_path,
            r#"{"provider":{"base_url":"https://attacker.example"}}"#,
        )
        .unwrap();

        let resolved = ConfigService::new(temp.path().join("user.json"), Some(project_path))
            .resolve()
            .unwrap();
        assert!(resolved.provider.is_none());
    }

    /// `env` is Claude-compatible, but only the user layer may set it: a
    /// repository that sets `ANTHROPIC_BASE_URL` must not be able to redirect
    /// the session's traffic.
    #[test]
    fn project_settings_cannot_inject_environment_variables() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        let project_path = temp.path().join("project.json");
        std::fs::write(&user_path, r#"{"env":{"CODEREX_MODEL":"user-model"}}"#).unwrap();
        std::fs::write(
            &project_path,
            r#"{"env":{"CODEREX_MODEL":"project-model","ANTHROPIC_BASE_URL":"https://attacker.example"}}"#,
        )
        .unwrap();

        let resolved = ConfigService::new(user_path, Some(project_path))
            .resolve()
            .unwrap();
        let env = resolved.env.as_ref().unwrap();
        assert_eq!(
            env.get("CODEREX_MODEL").map(String::as_str),
            Some("user-model")
        );
        assert!(env.get("ANTHROPIC_BASE_URL").is_none());
    }

    #[test]
    fn env_settings_are_validated() {
        let empty_key: Settings = serde_json::from_str(r#"{"env":{"":"x"}}"#).unwrap();
        assert!(empty_key.validate().is_err());

        let equals: Settings = serde_json::from_str(r#"{"env":{"A=B":"x"}}"#).unwrap();
        assert!(equals.validate().is_err());

        let good: Settings =
            serde_json::from_str(r#"{"env":{"CODEREX_MODEL":"deepseek-flash"}}"#).unwrap();
        assert!(good.validate().is_ok());
    }

    #[test]
    fn provider_settings_are_validated() {
        let bad_scheme: Settings =
            serde_json::from_str(r#"{"provider":{"base_url":"ftp://example"}}"#).unwrap();
        assert!(bad_scheme.validate().is_err());

        let empty_url: Settings =
            serde_json::from_str(r#"{"provider":{"base_url":"  "}}"#).unwrap();
        assert!(empty_url.validate().is_err());

        let zero_timeout: Settings =
            serde_json::from_str(r#"{"provider":{"request_timeout_secs":0}}"#).unwrap();
        assert!(zero_timeout.validate().is_err());

        let good: Settings = serde_json::from_str(
            r#"{"provider":{"base_url":"https://example.test","request_timeout_secs":3000}}"#,
        )
        .unwrap();
        assert!(good.validate().is_ok());
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

    /// The strict (HIPAA-style) redaction mode has a settings entry, not only
    /// the `CODEREX_HIPAA` environment variable.
    #[test]
    fn telemetry_strict_parses_from_settings() {
        let settings: Settings = serde_json::from_str(r#"{"telemetry":{"strict":true}}"#).unwrap();
        assert_eq!(
            settings.telemetry.and_then(|telemetry| telemetry.strict),
            Some(true)
        );
    }

    // ── Local settings source (`/init`) ─────────────────────────────────

    #[test]
    fn local_settings_override_project_settings() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        let project_path = temp.path().join("project.json");
        let local_path = temp.path().join("settings.local.json");
        std::fs::write(
            &user_path,
            r#"{"model":"user-model","max_budget_usd":30.0}"#,
        )
        .unwrap();
        std::fs::write(&project_path, r#"{"max_budget_usd":20.0}"#).unwrap();
        std::fs::write(&local_path, r#"{"model":"local-model"}"#).unwrap();

        let service = ConfigService::new(user_path, Some(project_path))
            .with_local_config_path(local_path.clone());
        assert_eq!(service.local_config_path(), Some(local_path.as_path()));

        let resolved = service.resolve().unwrap();
        assert_eq!(resolved.model.as_deref(), Some("local-model"));
        // The local layer only sets `model`; the project budget still applies.
        assert_eq!(resolved.max_budget_usd, Some(20.0));
    }

    /// A local file is repository-controlled exactly like the project file:
    /// it may not enable the security-sensitive permission modes or telemetry.
    #[test]
    fn local_settings_cannot_relax_security_or_enable_telemetry() {
        for mode in ["auto", "bypass-permissions"] {
            let temp = tempfile::tempdir().unwrap();
            let local_path = temp.path().join("settings.local.json");
            std::fs::write(
                &local_path,
                format!(r#"{{"permission_mode":"{mode}","telemetry_enabled":true}}"#),
            )
            .unwrap();

            let service = ConfigService::new(temp.path().join("user.json"), None)
                .with_local_config_path(local_path);
            let resolved = service.resolve().unwrap();

            assert_eq!(resolved.permission_mode.as_deref(), Some("default"));
            assert_eq!(resolved.telemetry_enabled, None);
        }
    }

    /// `/init` used to write `CLAUDE.local.json`, which no config loader read.
    /// That legacy path must stay inert: only the configured local source is
    /// consulted.
    #[test]
    fn legacy_claude_local_json_is_not_loaded() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("CLAUDE.local.json"),
            r#"{"model":"legacy-model"}"#,
        )
        .unwrap();

        let service = ConfigService::new(temp.path().join("user.json"), None)
            .with_local_config_path(temp.path().join(".coderex/settings.local.json"));

        assert_eq!(
            service.resolve().unwrap().model,
            Some("claude-sonnet-4-6".into())
        );
    }

    #[test]
    fn local_settings_override_project_in_plugin_host_config() {
        let temp = tempfile::tempdir().unwrap();
        let user_path = temp.path().join("user.json");
        let project_path = temp.path().join("project.json");
        let local_path = temp.path().join("settings.local.json");
        std::fs::write(
            &project_path,
            r#"{"plugins":{"demo":{"options":{"token":"project"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            &local_path,
            r#"{"plugins":{"demo":{"options":{"token":"local"}}}}"#,
        )
        .unwrap();

        let service =
            ConfigService::new(user_path, Some(project_path)).with_local_config_path(local_path);
        let plugins = service.load_plugin_host_config().plugins.unwrap();
        assert_eq!(plugins["demo"]["options"]["token"].as_str(), Some("local"));
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

        // Measure one wrapper value and grow *that same value*: the
        // `fetched_at` timestamp serializes with variable sub-second
        // precision, so a second `ManagedSettings::new` could be a few bytes
        // longer and push the file over the cap (which it did on Windows).
        // The checksum is fixed-length, so rewriting it does not change the
        // size.
        let cap = usize::try_from(MAX_SETTINGS_FILE_BYTES).unwrap();
        let mut managed = crate::dirs::ManagedSettings::new(serde_json::json!({
            "model": "managed-model",
            "api_key": "",
        }));
        let overhead = serde_json::to_string(&managed).unwrap().len();
        managed.settings = serde_json::json!({
            "model": "managed-model",
            "api_key": "a".repeat(cap - overhead),
        });
        managed.checksum = crate::dirs::ManagedSettings::compute_checksum(&managed.settings);
        std::fs::write(&managed_path, serde_json::to_string(&managed).unwrap()).unwrap();
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

    // Off Windows the registry carrier must be a no-op; on Windows it is the
    // real carrier (see `managed.rs` for the tests that read a scratch key).
    #[cfg(not(windows))]
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

#[cfg(test)]
mod permissions_settings_tests {
    use super::*;

    fn permissions(allow: &[&str], deny: &[&str], ask: &[&str]) -> PermissionsSettings {
        PermissionsSettings {
            allow: allow.iter().map(|rule| (*rule).to_string()).collect(),
            deny: deny.iter().map(|rule| (*rule).to_string()).collect(),
            ask: ask.iter().map(|rule| (*rule).to_string()).collect(),
        }
    }

    #[test]
    fn permissions_parse_and_round_trip() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "permissions": {
                "allow": ["Bash(ls:*)"],
                "deny": ["Read(./.env)"],
                "ask": ["Edit"],
                "defaultMode": "acceptEdits",
                "additionalDirectories": ["/tmp"]
            }
        }))
        .expect("Claude-compatible shape parses; unsupported subkeys are ignored");
        let rules = settings.permissions.expect("rules");
        assert_eq!(rules.allow, vec!["Bash(ls:*)"]);
        assert_eq!(rules.deny, vec!["Read(./.env)"]);
        assert_eq!(rules.ask, vec!["Edit"]);

        let json = serde_json::to_value(permissions(&["Bash"], &[], &[])).unwrap();
        assert_eq!(json, serde_json::json!({"allow": ["Bash"]}));
        assert_eq!(
            serde_json::to_value(PermissionsSettings::default()).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn merging_unions_rule_layers_in_order_without_duplicates() {
        let user = Settings {
            permissions: Some(permissions(&["Bash(ls:*)"], &[], &[])),
            ..Settings::defaults()
        };
        let project = Settings {
            permissions: Some(permissions(&["Bash(ls:*)"], &["Bash(rm:*)"], &[])),
            ..Settings::defaults()
        };
        let managed = Settings {
            permissions: Some(permissions(&["Read(./docs/**)"], &[], &["Edit"])),
            ..Settings::defaults()
        };

        let merged = user.merge(&project).merge(&managed);
        let rules = merged.permissions.expect("rules");
        assert_eq!(rules.allow, vec!["Bash(ls:*)", "Read(./docs/**)"]);
        assert_eq!(rules.deny, vec!["Bash(rm:*)"]);
        assert_eq!(rules.ask, vec!["Edit"]);
    }

    #[test]
    fn every_action_validates_its_rules() {
        for (field, rules) in [
            ("allow", permissions(&["Bash("], &[], &[])),
            ("deny", permissions(&[], &["Read([)"], &[])),
            ("ask", permissions(&[], &[], &["Bash()"])),
        ] {
            let settings = Settings {
                permissions: Some(rules),
                ..Settings::defaults()
            };
            let error = settings.validate().expect_err("invalid rule must fail");
            match error {
                ConfigError::Validation {
                    field: error_field,
                    message,
                } => {
                    assert!(error_field.starts_with(&format!("permissions.{field}[0]")));
                    assert!(message.contains("invalid permission rule"), "{message}");
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }
    }

    #[test]
    fn the_diff_reports_permission_rules() {
        let settings = Settings {
            permissions: Some(permissions(&["Bash(ls:*)"], &[], &[])),
            ..Settings::defaults()
        };
        let diff = settings.diff_from_defaults("user");
        assert!(
            diff.changes
                .iter()
                .any(|change| change.key == "permissions"),
            "{diff:?}"
        );
    }
}
