//! Plugin manager — loading, handles, marketplace reconciliation.

use dashmap::DashMap;
use ed25519_dalek::Verifier;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex as TokioMutex;

/// Plugin lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// Plugin manifest discovered but not loaded.
    Discovered,
    /// Plugin is being loaded.
    Loading,
    /// Plugin is active and its commands/skills are registered.
    Active,
    /// Plugin failed to load.
    Error(String),
    /// Plugin was uninstalled.
    Uninstalled,
    /// Plugin is being downloaded from marketplace.
    Installing,
}

/// A handle to a loaded plugin.
#[derive(Debug)]
pub struct PluginHandle {
    /// The plugin name.
    pub name: String,
    /// The plugin version.
    pub version: String,
    /// Current state.
    pub state: PluginState,
    /// Path to the plugin directory.
    pub path: PathBuf,
    /// Commands registered by this plugin.
    pub commands: Vec<String>,
    /// Skills registered by this plugin.
    pub skills: Vec<String>,
    /// Number of retry attempts for error recovery.
    pub retry_count: u32,
    /// Maximum retry attempts before giving up.
    pub max_retries: u32,
    /// Whether the installed archive matched a configured signing key.
    ///
    /// False means the code was installed without anything having been
    /// checked — either no key is configured or the plugin predates it.
    pub verified: bool,
    /// When the last recovery attempt was made, for the backoff between them.
    pub last_retry_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Plugin runtime type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeType {
    /// External process (JSON-RPC over stdio).
    Process,
    /// WASM module, run in-process by the `wasm-runtime` feature.
    Wasm,
}

impl Default for RuntimeType {
    fn default() -> Self {
        Self::Process
    }
}

impl std::fmt::Display for RuntimeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeType::Process => write!(f, "process"),
            RuntimeType::Wasm => write!(f, "wasm"),
        }
    }
}

/// A plugin author, as declared in `plugin.json`.
///
/// Claude's manifest writes this as an object (`{"name": ..., "email": ...}`)
/// and CodeRex manifests in the wild write it as a bare string. Both parse:
/// an object used to fail the whole manifest, which took the plugin with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PluginAuthor {
    /// Author name.
    pub name: String,
    /// Author contact address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl From<&str> for PluginAuthor {
    fn from(name: &str) -> Self {
        Self {
            name: name.to_string(),
            email: None,
        }
    }
}

impl From<String> for PluginAuthor {
    fn from(name: String) -> Self {
        Self { name, email: None }
    }
}

impl std::fmt::Display for PluginAuthor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl<'de> Deserialize<'de> for PluginAuthor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            /// `"author": "name"`.
            Name(String),
            /// `"author": {"name": ..., "email": ...}`.
            Full {
                name: String,
                #[serde(default)]
                email: Option<String>,
            },
        }

        Ok(match Repr::deserialize(deserializer)? {
            Repr::Name(name) => Self { name, email: None },
            Repr::Full { name, email } => Self { name, email },
        })
    }
}

/// One entry of a manifest's `userConfig` block.
///
/// The keys of the map are the option names, which become
/// `CODEREX_PLUGIN_OPTION_<KEY>` environment variables in the plugin process.
/// Claude's schema also allows `options`, `min` and `max`; they are accepted
/// so a Claude-shaped manifest parses, but CodeRex does not enforce them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfigField {
    /// Declared value type: `string`, `number`, `boolean`, `directory` or `file`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    /// Human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Human-readable explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the plugin needs this option to run.
    #[serde(default)]
    pub required: bool,
    /// Default value. Refused for `sensitive` options: a secret written into
    /// `plugin.json` is a secret in a file that gets committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Whether the value is a secret.
    #[serde(default)]
    pub sensitive: bool,
    /// Whether several values may be supplied.
    #[serde(default)]
    pub multiple: bool,
    /// Allowed values for a `string` option (accepted, not enforced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    /// Minimum for a `number` option (accepted, not enforced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<Value>,
    /// Maximum for a `number` option (accepted, not enforced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<Value>,
}

/// Environment variable prefix for plugin options: `<PREFIX><KEY>`.
pub const PLUGIN_OPTION_ENV_PREFIX: &str = "CODEREX_PLUGIN_OPTION_";

/// A plugin manifest — parsed from plugin.json.
///
/// Unknown fields are a hard parse error (`deny_unknown_fields`): a field a
/// plugin author invents, or misspells, is reported by name instead of being
/// silently dropped on the floor. Fields that belong to Claude's documented
/// manifest but that CodeRex does not act on yet are declared here anyway and
/// reported by [`PluginManifest::unimplemented_fields`], which is logged when
/// the plugin is loaded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// JSON schema URL (`$schema`), as written by the plugin scaffolding.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Plugin name.
    pub name: String,
    /// Human-readable name, for display.
    #[serde(
        rename = "displayName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<String>,
    /// Plugin version.
    #[serde(default)]
    pub version: String,
    /// Plugin description.
    #[serde(default)]
    pub description: String,
    /// Plugin author — a string or an object.
    #[serde(default)]
    pub author: PluginAuthor,
    /// Project homepage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Source repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// License identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Search keywords.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Whether the plugin is enabled without being asked for.
    #[serde(
        rename = "defaultEnabled",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub default_enabled: Option<bool>,
    /// Free-form metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Minimum CodeRex version required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    /// Commands this plugin provides.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Skills this plugin provides.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Other plugins this plugin depends on.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Ed25519 signature carried inside the manifest (hex-encoded).
    ///
    /// Advisory only: the manifest lives inside the archive it would have to
    /// sign, so a signature here can never be checked against the archive
    /// bytes. Archive installs use a detached signature instead — a
    /// `<archive>.sig` sidecar, or one supplied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Runtime type: "process" (default) or "wasm".
    #[serde(default)]
    pub runtime: RuntimeType,
    /// Entrypoint command for process plugins (e.g., "bin/plugin", "node index.js").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// Environment variables for the plugin process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Options the user configures, injected as `CODEREX_PLUGIN_OPTION_<KEY>`.
    #[serde(rename = "userConfig", default)]
    pub user_config: HashMap<String, UserConfigField>,

    // ── Accepted, not implemented ────────────────────────────────────────
    // Declared so a Claude-shaped manifest parses and so the fields are named
    // in the warning `unimplemented_fields()` produces, rather than vanishing.
    /// Hook matchers (CodeRex hooks come from settings, not from plugins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Value>,
    /// Agent definitions shipped by the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Value>,
    /// Output styles shipped by the plugin.
    #[serde(
        rename = "outputStyles",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_styles: Option<Value>,
    /// Themes shipped by the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Value>,
    /// Workflows shipped by the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflows: Option<Value>,
    /// Background monitor scripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitors: Option<Value>,
    /// MCP servers the plugin starts.
    #[serde(
        rename = "mcpServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mcp_servers: Option<Value>,
    /// LSP servers the plugin starts.
    #[serde(
        rename = "lspServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub lsp_servers: Option<Value>,
    /// Channels the plugin serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Value>,
    /// Binaries to fetch, with their SHA-256 pins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binaries: Option<Value>,
    /// Type declarations for the plugin's authoring surface.
    #[serde(rename = "types", default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Value>,
    /// Settings merged into the user's settings while the plugin is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    /// Experimental features (`themes`, `outputStyles`, `evals`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
}

impl PluginManifest {
    /// Fields this manifest declares that CodeRex accepts but does not act on.
    ///
    /// These are the documented Claude manifest components with no CodeRex
    /// counterpart yet. They parse (so the plugin still loads) but they have no
    /// effect, which the caller reports rather than leaving the author to guess.
    #[must_use]
    pub fn unimplemented_fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        let mut note = |present: bool, name: &'static str| {
            if present {
                fields.push(name);
            }
        };
        note(self.hooks.is_some(), "hooks");
        note(self.agents.is_some(), "agents");
        note(self.output_styles.is_some(), "outputStyles");
        note(self.themes.is_some(), "themes");
        note(self.workflows.is_some(), "workflows");
        note(self.monitors.is_some(), "monitors");
        note(self.mcp_servers.is_some(), "mcpServers");
        note(self.lsp_servers.is_some(), "lspServers");
        note(self.channels.is_some(), "channels");
        note(self.binaries.is_some(), "binaries");
        note(self.types.is_some(), "types");
        note(self.settings.is_some(), "settings");
        note(self.experimental.is_some(), "experimental");
        fields
    }

    /// Log the declared-but-unimplemented fields, if any.
    pub fn warn_unimplemented_fields(&self) {
        let fields = self.unimplemented_fields();
        if !fields.is_empty() {
            tracing::warn!(
                plugin = %self.name,
                fields = ?fields,
                "plugin.json declares fields CodeRex does not implement; they have no effect"
            );
        }
    }

    /// Resolve the values of the manifest's `userConfig` options.
    ///
    /// Resolution order for each declared option:
    /// 1. `provided` — a value the host has already resolved (from its own
    ///    configuration, or from the OS keychain: CodeRex's `keyring://`
    ///    convention lives in `coderex-config`).
    /// 2. `lookup` — normally CodeRex's own environment, for
    ///    `CODEREX_PLUGIN_OPTION_<KEY>`.
    /// 3. The manifest's `default`, which a `sensitive` option may not have.
    ///
    /// Errors are deliberate and loud, because every one of them is a way for a
    /// secret to end up somewhere it should not: an option name that cannot be
    /// an environment variable, a plaintext default for a sensitive option, an
    /// unresolved `keyring://` reference, or a required option with no value.
    pub fn resolve_options<F>(
        &self,
        provided: &HashMap<String, String>,
        lookup: F,
    ) -> Result<HashMap<String, String>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut values = HashMap::new();

        for (key, field) in &self.user_config {
            if !is_valid_option_key(key) {
                return Err(format!(
                    "userConfig option '{key}' is not a usable environment variable name \
                     (letters, digits and underscore, not starting with a digit)"
                ));
            }

            if field.sensitive && field.default.is_some() {
                return Err(format!(
                    "userConfig option '{key}' is sensitive but has a plaintext default in \
                     plugin.json; a secret belongs in the keychain or the environment, not in \
                     the manifest"
                ));
            }

            let env_key = format!("{PLUGIN_OPTION_ENV_PREFIX}{key}");
            let resolved = non_empty(provided.get(key).cloned())
                .or_else(|| non_empty(lookup(&env_key)))
                .or_else(|| {
                    field
                        .default
                        .as_ref()
                        .and_then(value_to_env_string)
                        .and_then(|v| non_empty(Some(v)))
                });

            let Some(value) = resolved else {
                if field.required {
                    return Err(format!(
                        "userConfig option '{key}' is required but no value was supplied \
                         (set {env_key} or configure the plugin)"
                    ));
                }
                tracing::debug!(
                    plugin = %self.name,
                    option = %key,
                    "optional userConfig option not supplied; not injecting it"
                );
                continue;
            };

            if field.sensitive && value.starts_with("keyring://") {
                return Err(format!(
                    "userConfig option '{key}' is sensitive and its value is still the \
                     keyring reference '{value}'; resolve it before handing it to a plugin \
                     (keychain access lives in coderex-config, behind its `keyring` feature)"
                ));
            }

            values.insert(key.clone(), value);
        }

        Ok(values)
    }

    /// Build the environment a plugin process is spawned with.
    ///
    /// The manifest's static `env` is carried over, with `${user_config.KEY}`
    /// substitution applied, and one `CODEREX_PLUGIN_OPTION_<KEY>` variable is
    /// added per resolved option. Sensitive values travel the same way, which
    /// is Claude's design too — they are in the child's environment, visible to
    /// the plugin and to the process table of the same user.
    #[must_use]
    pub fn build_env(&self, values: &HashMap<String, String>) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), substitute_user_config(v, values)))
            .collect();

        for (key, value) in values {
            env.insert(format!("{PLUGIN_OPTION_ENV_PREFIX}{key}"), value.clone());
        }

        for (key, value) in &env {
            if value.contains("${user_config.") {
                tracing::warn!(
                    plugin = %self.name,
                    variable = %key,
                    "environment variable still contains an unresolved \
                     ${{user_config.KEY}} placeholder"
                );
            }
        }

        env
    }
}

/// Whether an option name can be turned into an environment variable safely.
fn is_valid_option_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Treat an empty or blank value as "not supplied".
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Render a JSON default as an environment variable value.
///
/// Scalars stringify; a default that is an object or an array has no
/// environment-variable form and is not injected.
fn value_to_env_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Substitute `${user_config.KEY}` references with resolved option values.
///
/// Unknown keys are left as they were written, so a manifest author can see
/// what did not resolve rather than getting an empty string.
#[must_use]
pub fn substitute_user_config(text: &str, values: &HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (key, value) in values {
        out = out.replace(&format!("${{user_config.{key}}}"), value);
    }
    out
}

/// Environment variable carrying the plugin signing key.
pub const PLUGIN_PUBLIC_KEY_ENV: &str = "CODEREX_PLUGIN_PUBLIC_KEY";

/// What a signature check against the configured key concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureCheck {
    /// A configured key matched the archive.
    Verified,
    /// No key is configured, so nothing was checked.
    Unverified,
}

/// A callback a host registers to re-spawn a plugin that a retry recovered.
///
/// `retry_failed_plugins` runs synchronously and cannot await a spawn, so the
/// host can either spawn asynchronously here (for example with `tokio::spawn`)
/// and return `Ok(())`, or leave the handler unset and act on the
/// [`RetryOutcome::RestartRequired`] signal the retry returns.
#[derive(Clone)]
pub struct PluginRestartHandler {
    inner: Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>,
}

impl std::fmt::Debug for PluginRestartHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginRestartHandler")
    }
}

impl PluginRestartHandler {
    /// Wrap a restart callback.
    #[must_use]
    pub fn new(f: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self { inner: Arc::new(f) }
    }

    /// Invoke the callback for a plugin.
    pub fn call(&self, plugin: &str) -> Result<(), String> {
        (self.inner)(plugin)
    }
}

/// What a single retry of an errored plugin did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryOutcome {
    /// The manifest was readable again and the registered restart callback
    /// re-spawned the plugin successfully.
    Restarted(String),
    /// The manifest was readable again, but no restart callback is registered:
    /// the caller has to call [`PluginManager::spawn_plugin`] for this plugin.
    RestartRequired(String),
    /// The plugin is still in `Error`; the reason is in its state.
    StillFailed(String),
}

/// Marketplace plugin listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    /// Plugin name.
    pub name: String,
    /// Current version.
    pub version: String,
    /// Download URL for the plugin archive.
    pub url: String,
    /// Description.
    pub description: String,
}

/// Errors from marketplace operations.
#[derive(Debug, thiserror::Error)]
pub enum MarketplaceError {
    /// URL is not HTTPS.
    #[error("marketplace URL must use HTTPS: {0}")]
    InsecureUrl(String),
    /// URL is malformed.
    #[error("invalid marketplace URL: {0}")]
    InvalidUrl(String),
    /// URL is not in the allowlist.
    #[error("marketplace URL not in allowlist: {0}")]
    NotAllowed(String),
}

/// Marketplace connection state.
#[derive(Debug)]
pub struct MarketplaceConnection {
    /// The marketplace name.
    pub name: String,
    /// Base URL for the marketplace API.
    pub base_url: String,
    /// HTTP client for API calls.
    pub client: Client,
    /// Endpoint path for listing plugins (default: `/api/plugins`).
    pub list_endpoint: String,
    /// Endpoint path template for downloading a plugin (default: `/api/plugins/{name}/download`).
    pub download_endpoint: String,
}

/// Where a plugin is installed from.
///
/// CodeRex is local-first, so the source kinds that earn their place are the
/// ones that work without a package registry behind them: a directory on this
/// machine, a git repository, and a plain HTTPS archive. Marketplace listings
/// stay where they were, behind [`PluginManager::install_plugin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// A listing fetched from a registered marketplace over HTTPS.
    Marketplace {
        /// Listing name.
        name: String,
    },
    /// A directory on this machine holding a `plugin.json`.
    LocalDir(PathBuf),
    /// A git repository, cloned shallowly.
    Git {
        /// Clone URL.
        url: String,
        /// Branch, tag, or full commit SHA to check out.
        rev: Option<String>,
    },
    /// A subdirectory of a git repository, cloned and then narrowed to `path`.
    ///
    /// The reference's `git-subdir` source. The clone, transport validation and
    /// commit pinning are the git path's; the only addition is that the plugin
    /// directory is a named subdirectory rather than the repository root, and
    /// that name may not escape the clone.
    GitSubdir {
        /// Clone URL.
        url: String,
        /// Directory inside the repository that holds `plugin.json`.
        path: String,
        /// Branch or tag to check out (the reference's `ref`).
        rev: Option<String>,
        /// Full commit SHA to check out and pin to.
        sha: Option<String>,
    },
    /// A GitHub repository in `owner/repo` shorthand.
    ///
    /// The reference's `github` source. Resolved to
    /// `https://github.com/<owner>/<repo>.git` and installed by the git path, so
    /// the transport validation, `GIT_TERMINAL_PROMPT=0` and commit pinning all
    /// apply unchanged.
    Github {
        /// Repository in `owner/repo` form.
        repo: String,
        /// Branch or tag to check out (the reference's `ref`).
        rev: Option<String>,
        /// Full commit SHA to check out and pin to.
        sha: Option<String>,
    },
    /// A `plugin.tar.gz` (or `.tgz`) fetched over HTTPS.
    Archive {
        /// Archive URL.
        url: String,
        /// Expected SHA-256 of the archive, if the caller pinned one.
        sha256: Option<String>,
    },
    /// A source kind the reference defines but CodeRex does not implement.
    ///
    /// Recognised so the refusal is explicit rather than a fall-through to "a
    /// marketplace listing that could not be downloaded": `install_plugin`
    /// routes a listing whose url is not a marketplace URL straight here, so a
    /// third-party listing must not be able to name a source that executes.
    Unsupported {
        /// The recognised scheme (`npm`, `command`).
        kind: String,
    },
}

/// The `owner/repo` shorthand in a spec string becomes a git clone URL.
fn github_clone_url(repo: &str) -> Result<String, String> {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("github source '{repo}' is not in owner/repo form"));
    };
    for segment in [owner, name] {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!("github source '{repo}' is not in owner/repo form"));
        }
    }
    Ok(format!("https://github.com/{owner}/{name}.git"))
}

/// Split an optional `@rev` suffix off a spec's payload.
///
/// The *last* `@` wins so that a scp-like clone URL (`git@host:path`), which has
/// its own `@`, keeps its owner.
fn split_rev(spec: &str) -> (&str, Option<&str>) {
    match spec.rsplit_once('@') {
        Some((head, rev)) if !rev.is_empty() => (head, Some(rev)),
        _ => (spec, None),
    }
}

/// A revision that reads as a full commit SHA is a `sha`; anything else is a
/// branch or tag (`ref`).
fn classify_rev(rev: Option<&str>) -> (Option<String>, Option<String>) {
    match rev {
        Some(r) if is_full_commit_sha(r) => (None, Some(r.to_string())),
        Some(r) => (Some(r.to_string()), None),
        None => (None, None),
    }
}

/// Which revision to check out: an explicit commit wins over a branch or tag.
///
/// A `sha` that is not a full commit SHA is refused here rather than passed to
/// `git` as if it were a branch name — a caller that asked for a commit and
/// silently got a branch tip is the failure this avoids.
fn checkout_rev<'a>(rev: Option<&'a str>, sha: Option<&'a str>) -> Result<Option<&'a str>, String> {
    if let Some(sha) = sha {
        if !is_full_commit_sha(sha) {
            return Err(format!(
                "'{sha}' is not a full 40-character commit SHA, so it cannot be a source's sha"
            ));
        }
    }
    Ok(sha.or(rev))
}

/// Why an [`PluginSource::Unsupported`] kind is not implemented.
#[must_use]
pub fn unsupported_source_reason(kind: &str) -> String {
    match kind {
        // The reference's own message for this branch, quoted so the gap is
        // attributed rather than invented.
        "npm" => "NPM marketplace sources not yet implemented".to_string(),
        "command" => "a command source runs a command named by a marketplace entry; CodeRex \
                      has no sandbox for that and no consent step, so it is not implemented"
            .to_string(),
        other => format!("source kind '{other}' is not implemented"),
    }
}

impl PluginSource {
    /// Classify a source argument.
    ///
    /// An existing directory is a local source; something that looks like a git
    /// remote is cloned; an HTTPS URL ending in `.tar.gz`/`.tgz` is downloaded.
    /// Anything else is treated as a marketplace listing name, which is what
    /// `coderex plugin install <name>` has always meant.
    ///
    /// The reference's structured sources that a string cannot express by shape
    /// alone get a `scheme:` prefix: `github:owner/repo[@rev]` and
    /// `git-subdir:<url>#<path>[@rev]`. The schemes CodeRex deliberately does not
    /// implement (`npm:`, `command:`) are recognised so they refuse out loud
    /// instead of being mistaken for a marketplace plugin by that name.
    #[must_use]
    pub fn detect(spec: &str) -> Self {
        if let Some(rest) = spec.strip_prefix("github:") {
            let (repo, rev) = split_rev(rest);
            let (rev, sha) = classify_rev(rev);
            return Self::Github {
                repo: repo.to_string(),
                rev,
                sha,
            };
        }
        if let Some(rest) = spec.strip_prefix("git-subdir:") {
            let (url, tail) = rest.split_once('#').unwrap_or((rest, ""));
            let (path, rev) = split_rev(tail);
            let (rev, sha) = classify_rev(rev);
            return Self::GitSubdir {
                url: url.to_string(),
                path: path.to_string(),
                rev,
                sha,
            };
        }
        for scheme in ["npm", "command"] {
            if spec
                .strip_prefix(scheme)
                .is_some_and(|rest| rest.starts_with(':'))
            {
                return Self::Unsupported {
                    kind: scheme.to_string(),
                };
            }
        }
        if std::path::Path::new(spec).is_dir() {
            return Self::LocalDir(PathBuf::from(spec));
        }
        if looks_like_git_url(spec) {
            return Self::Git {
                url: spec.to_string(),
                rev: None,
            };
        }
        if spec.starts_with("https://")
            && (ends_with_ignore_case(spec, ".tar.gz") || ends_with_ignore_case(spec, ".tgz"))
        {
            return Self::Archive {
                url: spec.to_string(),
                sha256: None,
            };
        }
        Self::Marketplace {
            name: spec.to_string(),
        }
    }

    /// Whether this source needs a marketplace connection.
    #[must_use]
    pub const fn is_marketplace(&self) -> bool {
        matches!(self, Self::Marketplace { .. })
    }
}

/// Whether a string looks like a git remote rather than a plugin name.
fn looks_like_git_url(spec: &str) -> bool {
    const GIT_SCHEMES: [&str; 5] = ["https://", "ssh://", "git://", "git+https://", "file://"];
    if GIT_SCHEMES.iter().any(|s| spec.starts_with(s)) {
        let lowered = spec.to_ascii_lowercase();
        // Only the well-known forges are guessed at without a `.git` suffix;
        // anything else needs the suffix to be recognised.
        return ends_with_ignore_case(spec, ".git")
            || lowered.contains("github.com/")
            || lowered.contains("gitlab.com/")
            || lowered.contains("codeberg.org/")
            || lowered.contains("bitbucket.org/");
    }
    is_scp_like_git_url(spec)
}

/// Whether `text` ends with `suffix`, ignoring ASCII case.
fn ends_with_ignore_case(text: &str, suffix: &str) -> bool {
    text.get(text.len().saturating_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// Whether a string uses git's `user@host:path` shorthand.
fn is_scp_like_git_url(spec: &str) -> bool {
    let Some((user_host, path)) = spec.split_once(':') else {
        return false;
    };
    !path.is_empty()
        && !user_host.is_empty()
        && user_host.contains('@')
        && !user_host.contains('/')
        && !user_host.contains(' ')
}

/// Refuse git transports that run commands.
///
/// `git clone ext::sh -c '...'` runs an arbitrary command, so the transport is
/// checked before `git` is ever spawned. Only transports that fetch from a
/// remote (or from a local path) are allowed.
fn validate_git_url(url: &str) -> Result<(), String> {
    // `ext::` (and friends) hand the "URL" to a shell. The transport prefix is
    // whatever precedes the first `::`, so a plain `https://` URL is unaffected.
    if let Some((transport, _)) = url.split_once("::") {
        let named_transport = !transport.is_empty()
            && transport
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-');
        if named_transport {
            return Err(format!(
                "refusing git URL '{url}': the {transport}:: transport can run arbitrary \
                 commands"
            ));
        }
    }
    let allowed = url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.starts_with("git://")
        || url.starts_with("git+https://")
        || url.starts_with("file://")
        || is_scp_like_git_url(url);
    if !allowed {
        return Err(format!(
            "refusing git URL '{url}': expected https://, ssh://, git://, file:// or \
             user@host:path"
        ));
    }
    Ok(())
}

/// A recorded pin for an installed plugin.
///
/// Trust-on-first-use: the first install records what it got, and later
/// installs of the same plugin have to match it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPin {
    /// SHA-256 of the installed archive, when the plugin came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Commit the plugin was cloned at, when it came from git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
}

impl PluginPin {
    /// Whether this pin records anything.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sha256.is_none() && self.git_commit.is_none()
    }

    /// Compare a candidate pin against this one.
    ///
    /// Only the fields recorded here are checked, so a plugin pinned by commit
    /// is not silently re-checked against a hash it never had.
    pub fn check(&self, candidate: &Self) -> Result<(), String> {
        if let Some(expected) = &self.sha256 {
            match &candidate.sha256 {
                Some(actual) if actual == expected => {}
                Some(actual) => {
                    return Err(format!(
                        "plugin archive does not match its pinned SHA-256 (pinned {expected}, \
                         got {actual})"
                    ));
                }
                None => return Err("plugin no longer comes from an archive to hash".into()),
            }
        }
        if let Some(expected) = &self.git_commit {
            match &candidate.git_commit {
                Some(actual) if actual == expected => {}
                Some(actual) => {
                    return Err(format!(
                        "plugin does not match its pinned commit (pinned {expected}, got {actual})"
                    ));
                }
                None => return Err("plugin no longer comes from a git checkout".into()),
            }
        }
        Ok(())
    }
}

/// The on-disk record of pinned plugin contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginPinStore {
    /// Plugin name to pin.
    #[serde(default)]
    pub plugins: HashMap<String, PluginPin>,
}

impl PluginPinStore {
    /// Read the pin file, if it exists.
    ///
    /// A pin file that cannot be parsed is an error rather than an empty store:
    /// silently forgetting the pins would turn the next install into a fresh
    /// first use.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read pin file {}: {e}", path.display()))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("failed to parse pin file {}: {e}", path.display()))
    }

    /// Write the pin file.
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize pin file: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| format!("failed to write pin file {}: {e}", path.display()))
    }

    /// The pin recorded for a plugin, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PluginPin> {
        self.plugins.get(name)
    }
}

/// The pin file name inside an install directory.
pub const PIN_FILE_NAME: &str = ".coderex-pins.json";

/// SHA-256, hex-encoded.
///
/// The workspace has `sha2`, but this crate cannot take a dependency on it
/// without touching `Cargo.toml` and `Cargo.lock`, which are shared. The
/// function is the published algorithm verbatim and is checked against the
/// NIST vectors in the tests; it is used for integrity pinning only, never as a
/// signature. Swap it for `sha2` the moment this crate may add dependencies.
#[must_use]
// The working variables carry the names the specification gives them.
#[allow(clippy::many_single_char_names)]
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut bytes = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    hex::encode(bytes)
}

/// Whether a revision is a full commit SHA rather than a branch or tag name.
fn is_full_commit_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

/// Run a git command and return its standard output.
///
/// Credential prompts are turned off: a plugin install that cannot authenticate
/// fails with a message instead of hanging on a prompt nobody sees.
async fn run_git(args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::from(coderex_core::git_hardening::git_command())
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Never inherit a repository selection from the caller: an install must
        // operate on the URL it was given, not on whatever repo the process
        // happens to be pointed at.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .await
        .map_err(|e| format!("failed to run git: {e} (is git installed?)"))?;

    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Copy a directory tree, skipping anything that is not a regular file or
/// directory.
fn copy_dir_recursive(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("failed to create {}: {e}", to.display()))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| format!("failed to read {}: {e}", from.display()))?;
    for entry in entries.flatten() {
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| format!("failed to stat {}: {e}", source.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source, &target)?;
        } else if file_type.is_file() {
            std::fs::copy(&source, &target)
                .map_err(|e| format!("failed to copy {}: {e}", source.display()))?;
        }
    }
    Ok(())
}

use crate::runtime::PluginProcess;
use crate::wasm_runtime::PluginWasmInstance;

/// Manages all loaded plugins and marketplace state.
#[derive(Debug)]
pub struct PluginManager {
    /// Loaded plugins.
    plugins: DashMap<String, PluginHandle>,
    /// Active plugin processes (for process-runtime plugins).
    pub(crate) active_processes: DashMap<String, PluginProcess>,
    /// Active WASM plugin instances (for wasm-runtime plugins).
    pub(crate) active_wasm_instances: DashMap<String, PluginWasmInstance>,
    /// Marketplace connections.
    marketplaces: DashMap<String, MarketplaceConnection>,
    /// Whether a refresh is needed (new plugins installed).
    needs_refresh: AtomicBool,
    /// Plugin base directories.
    plugin_dirs: Vec<PathBuf>,
    /// Optional allowlist of trusted marketplace domains.
    marketplace_allowlist: Vec<String>,
    /// Current application version for compatibility checks.
    app_version: String,
    /// Install locks — prevents concurrent installs of the same plugin.
    install_locks: DashMap<String, Arc<TokioMutex<()>>>,
    /// Optional Ed25519 public key for signature verification (hex-encoded 32 bytes).
    public_key: Option<String>,
    /// `userConfig` values the host has resolved, per plugin.
    plugin_options: DashMap<String, HashMap<String, String>>,
    /// Callback that re-spawns a plugin a retry recovered, if the host set one.
    restart_handler: Option<PluginRestartHandler>,
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginManager {
    /// Create a new plugin manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: DashMap::new(),
            active_processes: DashMap::new(),
            active_wasm_instances: DashMap::new(),
            marketplaces: DashMap::new(),
            needs_refresh: AtomicBool::new(false),
            plugin_dirs: Vec::new(),
            marketplace_allowlist: Vec::new(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            install_locks: DashMap::new(),
            public_key: None,
            plugin_options: DashMap::new(),
            restart_handler: None,
        }
    }

    /// Set the application version for compatibility checks.
    pub fn set_app_version(&mut self, version: impl Into<String>) {
        self.app_version = version.into();
    }

    /// Supply a value for one of a plugin's `userConfig` options.
    ///
    /// The host resolves these — from its own configuration, or from the OS
    /// keychain, which is why a `sensitive` value arriving here as an
    /// unresolved `keyring://` reference is refused rather than injected.
    pub fn set_plugin_option(&self, plugin: &str, key: &str, value: impl Into<String>) {
        self.plugin_options
            .entry(plugin.to_string())
            .or_default()
            .insert(key.to_string(), value.into());
    }

    /// The `userConfig` values supplied for a plugin.
    #[must_use]
    pub fn plugin_options(&self, plugin: &str) -> HashMap<String, String> {
        self.plugin_options
            .get(plugin)
            .map(|e| e.value().clone())
            .unwrap_or_default()
    }

    /// Apply host-resolved `userConfig` values for every plugin at once.
    ///
    /// This is the settings-backed entry point: the host reads its own
    /// configuration (a settings file, the keychain, or an environment it
    /// controls), resolves the values, and hands the result here. Values are
    /// merged into whatever was already supplied, with the map winning.
    pub fn apply_options_from_map(&self, options: &HashMap<String, HashMap<String, String>>) {
        for (plugin, values) in options {
            let mut entry = self.plugin_options.entry(plugin.clone()).or_default();
            for (key, value) in values {
                entry.insert(key.clone(), value.clone());
            }
        }
    }

    /// Apply host-resolved values for one plugin, replacing the previous set.
    pub fn apply_options(&self, plugin: &str, values: impl IntoIterator<Item = (String, String)>) {
        let values: HashMap<String, String> = values.into_iter().collect();
        self.plugin_options.insert(plugin.to_string(), values);
    }

    /// Apply `userConfig` values from a settings-shaped JSON document.
    ///
    /// Accepts `{"plugins": {"<plugin>": {"options": {...}}}}`, which is the
    /// shape a settings file uses for per-plugin options, and the flatter
    /// `{"pluginOptions": {"<plugin>": {...}}}`. Scalar values are stringified
    /// exactly as they would be for the environment; an object or array is an
    /// error, because it has no environment-variable form and silently dropping
    /// it would leave the plugin thinking an option was configured.
    ///
    /// Returns how many values were applied.
    pub fn apply_options_from_settings(&self, settings: &Value) -> Result<usize, String> {
        let per_plugin = settings
            .get("plugins")
            .and_then(|plugins| plugins.as_object())
            .map(|plugins| {
                plugins
                    .iter()
                    .filter_map(|(name, entry)| {
                        let options = entry.get("options")?;
                        Some((name.clone(), options))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                settings
                    .get("pluginOptions")
                    .and_then(Value::as_object)
                    .map(|map| {
                        map.iter()
                            .map(|(name, options)| (name.clone(), options))
                            .collect()
                    })
                    .unwrap_or_default()
            });

        let mut applied = 0;
        for (plugin, options) in per_plugin {
            let options = options.as_object().ok_or_else(|| {
                format!("settings for plugin '{plugin}' must map option names to scalar values")
            })?;
            let mut values = HashMap::new();
            for (key, value) in options {
                let text = value_to_env_string(value).ok_or_else(|| {
                    format!("plugin '{plugin}' option '{key}' must be a string, number or boolean")
                })?;
                values.insert(key.clone(), text);
                applied += 1;
            }
            let mut entry = self.plugin_options.entry(plugin).or_default();
            entry.extend(values);
        }
        Ok(applied)
    }

    /// Register the callback a successful [`Self::retry_failed_plugins`] uses
    /// to re-spawn a plugin.
    ///
    /// A retry that only marks a plugin `Active` without spawning it is a
    /// plugin CodeRex reports as loaded but cannot run. With a handler set, the
    /// plugin becomes `Active` only after the handler returns `Ok(())`.
    pub fn set_restart_handler(
        &mut self,
        handler: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    ) {
        self.restart_handler = Some(PluginRestartHandler::new(handler));
    }

    /// Remove the restart callback, if any.
    pub fn clear_restart_handler(&mut self) {
        self.restart_handler = None;
    }

    /// Set the marketplace domain allowlist.
    pub fn set_marketplace_allowlist(&mut self, domains: Vec<String>) {
        self.marketplace_allowlist = domains;
    }

    /// Add a plugin directory to search.
    pub fn add_plugin_dir(&mut self, path: PathBuf) {
        self.plugin_dirs.push(path);
    }

    /// Discover plugins in registered directories.
    pub fn discover(&self) -> Vec<PluginManifest> {
        let mut found = Vec::new();
        for dir in &self.plugin_dirs {
            if !dir.exists() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let manifest_path = entry.path().join("plugin.json");
                    if manifest_path.exists() {
                        // A manifest that does not parse is reported, not
                        // skipped in silence: the plugin simply not appearing
                        // is the hardest failure for an author to diagnose.
                        match std::fs::read_to_string(&manifest_path)
                            .map_err(|e| e.to_string())
                            .and_then(|content| {
                                serde_json::from_str::<PluginManifest>(&content)
                                    .map_err(|e| e.to_string())
                            }) {
                            Ok(manifest) => found.push(manifest),
                            Err(e) => tracing::warn!(
                                manifest = %manifest_path.display(),
                                "ignoring plugin: {e}"
                            ),
                        }
                    }
                }
            }
        }
        found
    }

    /// Load a discovered plugin.
    ///
    /// Skips loading if the plugin's `min_version` is incompatible.
    /// For process-runtime plugins, spawns the subprocess asynchronously.
    pub fn load_plugin(&self, manifest: PluginManifest, path: PathBuf) {
        manifest.warn_unimplemented_fields();

        if !self.is_version_compatible(manifest.min_version.as_deref()) {
            tracing::warn!(
                plugin = %manifest.name,
                min_version = ?manifest.min_version,
                app_version = %self.app_version,
                "skipping plugin: incompatible version"
            );
            let handle = PluginHandle {
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                state: PluginState::Error(format!(
                    "requires CodeRex >= {} (current: {})",
                    manifest.min_version.as_deref().unwrap_or("unknown"),
                    self.app_version
                )),
                path: path.clone(),
                commands: Vec::new(),
                skills: Vec::new(),
                retry_count: 0,
                max_retries: 3,
                verified: false,
                last_retry_at: None,
            };
            self.plugins.insert(handle.name.clone(), handle);
            return;
        }

        let handle = PluginHandle {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            state: PluginState::Active,
            path: path.clone(),
            commands: manifest.commands.clone(),
            skills: manifest.skills.clone(),
            retry_count: 0,
            max_retries: 3,
            // A plugin loaded from a directory has no archive to check, so
            // nothing about it was verified. Only an install against a
            // configured key sets this.
            verified: false,
            last_retry_at: None,
        };
        self.plugins.insert(manifest.name.clone(), handle);

        // Spawn process plugins asynchronously (only if a tokio runtime is available)
        if manifest.runtime == RuntimeType::Process {
            let name = manifest.name.clone();
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::spawn(async move {
                    tracing::debug!(
                        plugin = %name,
                        "process plugin loaded; spawn via PluginManager::spawn_plugin()"
                    );
                });
            }
        }
    }

    /// Unload a plugin, stop its runtime, and clean up its directory.
    pub fn unload_plugin(&self, name: &str) -> bool {
        // Stop the process first (only if a tokio runtime is available)
        if let Some((_, mut process)) = self.active_processes.remove(name) {
            if tokio::runtime::Handle::try_current().is_ok() {
                let _ = tokio::spawn(async move {
                    let _ = process.terminate().await;
                });
            }
        }

        // Remove WASM instance if present
        let _ = self.active_wasm_instances.remove(name);

        if let Some((_, handle)) = self.plugins.remove(name) {
            // Clean up plugin directory if it exists
            if handle.path.exists() {
                if let Err(e) = std::fs::remove_dir_all(&handle.path) {
                    tracing::warn!("failed to remove plugin directory for '{}': {e}", name);
                }
            }
            self.needs_refresh.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Spawn a process plugin's subprocess and perform initialize handshake.
    ///
    /// Discovers commands and skills dynamically from the plugin process.
    ///
    /// A failure is recorded on the plugin as [`PluginState::Error`], which is
    /// what gives [`Self::retry_failed_plugins`] something real to retry. A
    /// success marks the plugin `Active`.
    pub async fn spawn_plugin(&self, name: &str) -> Result<(), String> {
        match self.spawn_plugin_inner(name).await {
            Ok(()) => {
                if let Some(mut entry) = self.plugins.get_mut(name) {
                    entry.state = PluginState::Active;
                    entry.retry_count = 0;
                }
                Ok(())
            }
            Err(e) => {
                self.set_plugin_state(name, PluginState::Error(e.clone()));
                Err(e)
            }
        }
    }

    async fn spawn_plugin_inner(&self, name: &str) -> Result<(), String> {
        let handle = self
            .plugins
            .get(name)
            .ok_or_else(|| format!("plugin '{}' not found", name))?;

        let manifest_path = handle.path.join("plugin.json");
        let manifest: PluginManifest = if manifest_path.exists() {
            let content = std::fs::read_to_string(&manifest_path)
                .map_err(|e| format!("failed to read plugin.json: {e}"))?;
            serde_json::from_str(&content)
                .map_err(|e| format!("failed to parse plugin.json: {e}"))?
        } else {
            return Err(format!("plugin '{}' missing plugin.json", name));
        };

        if manifest.runtime == RuntimeType::Wasm {
            // Resolve `userConfig` exactly as for a process plugin. The
            // values are handed to the module through the `coderex.user_config`
            // import rather than an environment, because a WASM module has no
            // environment. Errors (a missing required option, a plaintext
            // default for a secret, an unresolved `keyring://` reference) stop
            // the spawn before anything is compiled.
            let provided = self.plugin_options(name);
            let option_values = manifest
                .resolve_options(&provided, |key| std::env::var(key).ok())
                .map_err(|e| format!("plugin '{name}' userConfig: {e}"))?;

            let wasm_file = manifest.entrypoint.as_deref().unwrap_or("plugin.wasm");
            let wasm_path = handle.path.join(wasm_file);

            let mut instance = PluginWasmInstance::new_with_options(&wasm_path, option_values)
                .map_err(|e| format!("failed to load WASM plugin '{name}': {e}"))?;

            instance
                .initialize()
                .await
                .map_err(|e| format!("WASM plugin '{name}' initialize failed: {e}"))?;

            let discovered_commands: Vec<String> = match instance.list_commands().await {
                Ok(cmds) => cmds.into_iter().map(|c| c.name).collect(),
                Err(e) => {
                    tracing::warn!(plugin = %name, "failed to list commands: {e}");
                    manifest.commands.clone()
                }
            };

            let discovered_skills: Vec<String> = match instance.list_skills().await {
                Ok(skills) => skills.into_iter().map(|s| s.name).collect(),
                Err(e) => {
                    tracing::warn!(plugin = %name, "failed to list skills: {e}");
                    manifest.skills.clone()
                }
            };

            drop(handle);
            if let Some(mut entry) = self.plugins.get_mut(name) {
                entry.commands = discovered_commands;
                entry.skills = discovered_skills;
            }

            self.active_wasm_instances
                .insert(name.to_string(), instance);
            tracing::info!(plugin = %name, "WASM plugin loaded successfully");
            return Ok(());
        }

        if manifest.runtime != RuntimeType::Process {
            return Err(format!(
                "plugin '{}' has unsupported runtime '{}'",
                name, manifest.runtime
            ));
        }

        // Resolve userConfig, then build the environment and the entrypoint
        // from it. A required option with no value, or a sensitive option
        // configured in a way we will not accept, stops the spawn: a plugin
        // that silently runs without the secret it asked for is worse than one
        // that does not run at all.
        let provided = self.plugin_options(name);
        let option_values = manifest
            .resolve_options(&provided, |key| std::env::var(key).ok())
            .map_err(|e| format!("plugin '{name}' userConfig: {e}"))?;
        let env = manifest.build_env(&option_values);

        // Determine entrypoint
        let entrypoint = manifest.entrypoint.as_deref().unwrap_or("bin/plugin");
        let entrypoint = substitute_user_config(entrypoint, &option_values);

        // Split entrypoint into command and args (supports "node index.js")
        let parts: Vec<&str> = entrypoint.split_whitespace().collect();
        if parts.is_empty() {
            return Err(format!("plugin '{}' has empty entrypoint", name));
        }

        let cmd = parts[0].to_string();
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        let mut process = PluginProcess::new(cmd, args, env, handle.path.clone());

        process.spawn().await?;
        process.initialize().await?;

        // Discover commands and skills dynamically
        let discovered_commands: Vec<String> = match process.list_commands().await {
            Ok(cmds) => cmds.into_iter().map(|c| c.name).collect(),
            Err(e) => {
                tracing::warn!(plugin = %name, "failed to list commands: {e}");
                manifest.commands.clone()
            }
        };

        let discovered_skills: Vec<String> = match process.list_skills().await {
            Ok(skills) => skills.into_iter().map(|s| s.name).collect(),
            Err(e) => {
                tracing::warn!(plugin = %name, "failed to list skills: {e}");
                manifest.skills.clone()
            }
        };

        // Update handle with discovered commands/skills
        drop(handle);
        if let Some(mut entry) = self.plugins.get_mut(name) {
            entry.commands = discovered_commands;
            entry.skills = discovered_skills;
        }

        self.active_processes.insert(name.to_string(), process);
        tracing::info!(plugin = %name, "process plugin spawned successfully");
        Ok(())
    }

    /// Stop a plugin's runtime (process or wasm).
    pub async fn stop_plugin(&self, name: &str) -> bool {
        let mut stopped = false;

        if let Some((_, mut process)) = self.active_processes.remove(name) {
            if let Err(e) = process.terminate().await {
                tracing::warn!(plugin = %name, "failed to terminate process: {e}");
            }
            stopped = true;
        }

        if self.active_wasm_instances.remove(name).is_some() {
            stopped = true;
        }

        stopped
    }

    /// Find the first active plugin that provides the given command.
    #[must_use]
    pub fn find_plugin_for_command(&self, command_name: &str) -> Option<String> {
        for entry in self.plugins.iter() {
            if entry.commands.iter().any(|c| c == command_name) {
                return Some(entry.key().clone());
            }
        }
        None
    }

    /// Execute a command on an active plugin (process or wasm).
    pub async fn execute_command(
        &self,
        plugin_name: &str,
        command_name: &str,
        args: &[String],
    ) -> Result<String, String> {
        if let Some(mut entry) = self.active_processes.get_mut(plugin_name) {
            return entry.execute_command(command_name, args).await;
        }
        if let Some(mut entry) = self.active_wasm_instances.get_mut(plugin_name) {
            return entry.execute_command(command_name, args).await;
        }
        Err(format!(
            "plugin '{}' is not running (no active process or wasm instance)",
            plugin_name
        ))
    }

    /// Execute a skill on an active plugin (process or wasm).
    pub async fn execute_skill(
        &self,
        plugin_name: &str,
        skill_name: &str,
        context: &Value,
    ) -> Result<String, String> {
        if let Some(mut entry) = self.active_processes.get_mut(plugin_name) {
            return entry.execute_skill(skill_name, context).await;
        }
        if let Some(mut entry) = self.active_wasm_instances.get_mut(plugin_name) {
            return entry.execute_skill(skill_name, context).await;
        }
        Err(format!(
            "plugin '{}' is not running (no active process or wasm instance)",
            plugin_name
        ))
    }

    /// Mark that a refresh is needed.
    pub fn mark_needs_refresh(&self) {
        self.needs_refresh.store(true, Ordering::SeqCst);
    }

    /// Clear the refresh flag.
    pub fn clear_refresh_flag(&self) {
        self.needs_refresh.store(false, Ordering::SeqCst);
    }

    /// Check if a refresh is needed.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        self.needs_refresh.load(Ordering::Relaxed)
    }

    /// Get a plugin by name.
    #[must_use]
    pub fn get_plugin(&self, name: &str) -> Option<PluginHandle> {
        self.plugins.get(name).map(|e| {
            let h = e.value();
            PluginHandle {
                name: h.name.clone(),
                version: h.version.clone(),
                state: h.state.clone(),
                path: h.path.clone(),
                commands: h.commands.clone(),
                skills: h.skills.clone(),
                retry_count: h.retry_count,
                max_retries: h.max_retries,
                verified: h.verified,
                last_retry_at: h.last_retry_at,
            }
        })
    }

    /// List all loaded plugin names.
    #[must_use]
    pub fn list_plugins(&self) -> Vec<String> {
        self.plugins.iter().map(|e| e.key().clone()).collect()
    }

    /// Count loaded plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Register a marketplace connection.
    ///
    /// Validates that the URL is HTTPS and, if an allowlist is configured,
    /// that the domain is trusted.
    pub fn register_marketplace(
        &self,
        name: String,
        base_url: String,
    ) -> Result<(), MarketplaceError> {
        // Parse and validate URL structure
        let parsed =
            url::Url::parse(&base_url).map_err(|e| MarketplaceError::InvalidUrl(format!("{e}")))?;

        // Enforce HTTPS
        if parsed.scheme() != "https" {
            return Err(MarketplaceError::InsecureUrl(base_url));
        }

        // Check allowlist if configured
        if !self.marketplace_allowlist.is_empty() {
            let host = parsed.host_str().unwrap_or("");
            let allowed = self
                .marketplace_allowlist
                .iter()
                .any(|d| host == d.as_str() || host.ends_with(&format!(".{d}")));
            if !allowed {
                return Err(MarketplaceError::NotAllowed(base_url));
            }
        }

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        self.marketplaces.insert(
            name.clone(),
            MarketplaceConnection {
                name,
                base_url,
                client,
                list_endpoint: "/api/plugins".into(),
                download_endpoint: "/api/plugins/{name}/download".into(),
            },
        );
        Ok(())
    }

    /// Register a marketplace with custom endpoint paths.
    pub fn register_marketplace_with_endpoints(
        &self,
        name: String,
        base_url: String,
        list_endpoint: String,
        download_endpoint: String,
    ) -> Result<(), MarketplaceError> {
        self.register_marketplace(name.clone(), base_url)?;
        if let Some(mut entry) = self.marketplaces.get_mut(&name) {
            entry.list_endpoint = list_endpoint;
            entry.download_endpoint = download_endpoint;
        }
        Ok(())
    }

    /// Fetch available plugins from the marketplace.
    pub async fn fetch_marketplace_plugins(&self) -> Result<Vec<MarketplacePlugin>, String> {
        // Try each registered marketplace
        for entry in self.marketplaces.iter() {
            let conn = entry.value();
            let url = format!("{}{}", conn.base_url, conn.list_endpoint);

            match conn.client.get(&url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        match response.json::<Vec<MarketplacePlugin>>().await {
                            Ok(plugins) => {
                                tracing::info!(
                                    "fetched {} plugins from marketplace '{}'",
                                    plugins.len(),
                                    conn.name
                                );
                                return Ok(plugins);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "failed to parse marketplace response from '{}': {e}",
                                    conn.name
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            "marketplace '{}' returned status {}",
                            conn.name,
                            response.status()
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to connect to marketplace '{}': {e}", conn.name);
                }
            }
        }

        // Fall back to local reconciliation if no marketplace is available
        Ok(Vec::new())
    }

    /// Check if a plugin's minimum version is compatible with the current app version.
    fn is_version_compatible(&self, min_version: Option<&str>) -> bool {
        let min = match min_version {
            Some(v) => v,
            None => return true,
        };
        match (
            semver::Version::parse(min),
            semver::Version::parse(&self.app_version),
        ) {
            (Ok(min_v), Ok(app_v)) => app_v >= min_v,
            _ => {
                tracing::warn!("failed to parse version strings for compatibility check");
                true // fail-open on parse error
            }
        }
    }

    /// Extract a `.tar.gz` archive into the given directory.
    fn extract_archive(archive_bytes: &[u8], dest: &std::path::Path) -> Result<(), String> {
        let gz = flate2::read::GzDecoder::new(archive_bytes);
        let mut archive = tar::Archive::new(gz);
        archive
            .unpack(dest)
            .map_err(|e| format!("failed to extract archive: {e}"))?;
        Ok(())
    }

    /// Validate that an extracted plugin directory contains a valid `plugin.json`.
    fn validate_extracted_plugin(dir: &std::path::Path) -> Result<PluginManifest, String> {
        Self::read_manifest_dir(dir)
    }

    /// A staging directory for a source, next to the final location so the
    /// final move is a rename on the same filesystem.
    fn staging_dir(install_dir: &Path, tag: &str) -> PathBuf {
        let safe: String = tag
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        install_dir.join(format!(".install-{safe}"))
    }

    /// Read and parse the `plugin.json` of a plugin directory.
    fn read_manifest_dir(dir: &Path) -> Result<PluginManifest, String> {
        let manifest_path = dir.join("plugin.json");
        if !manifest_path.exists() {
            return Err(format!(
                "plugin directory {} has no plugin.json",
                dir.display()
            ));
        }
        let content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("failed to read plugin.json: {e}"))?;
        serde_json::from_str(&content).map_err(|e| format!("failed to parse plugin.json: {e}"))
    }

    /// Refuse a plugin name that would escape the install directory.
    ///
    /// The name comes from the plugin's own manifest, so an archive must not be
    /// able to name itself `../../etc` and have the install write there.
    fn validate_plugin_name(name: &str) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("plugin.json has an empty name".into());
        }
        if name.contains('/') || name.contains('\\') || name.contains('\0') || name == "." {
            return Err(format!(
                "plugin name '{name}' is not usable as a directory name"
            ));
        }
        Ok(())
    }

    /// Refuse an install whose `min_version` the running CodeRex does not meet.
    fn check_install_compatible(&self, manifest: &PluginManifest) -> Result<(), String> {
        if self.is_version_compatible(manifest.min_version.as_deref()) {
            return Ok(());
        }
        Err(format!(
            "plugin '{}' requires CodeRex >= {} (current: {})",
            manifest.name,
            manifest.min_version.as_deref().unwrap_or("unknown"),
            self.app_version
        ))
    }

    /// Check a source that has no archive signature to verify.
    ///
    /// Without a configured key the old behaviour stands: say that nothing was
    /// checked and install. With a key configured, an unverifiable source is
    /// accepted only when a previous trust-on-first-use install pinned exactly
    /// these contents. Otherwise the install fails closed.
    fn check_unverifiable_source(
        &self,
        name: &str,
        kind: &str,
        install_dir: &Path,
        pin: &PluginPin,
    ) -> Result<(), String> {
        if self.public_key.is_none() {
            tracing::warn!(
                plugin = %name,
                source = kind,
                "no signing key is configured, so this {kind} source cannot be verified; \
                 this plugin is installed unverified"
            );
            return Ok(());
        }

        let store = PluginPinStore::load(&install_dir.join(PIN_FILE_NAME))?;
        let Some(existing) = store.get(name) else {
            return Err(format!(
                "a signing key is configured, but a {kind} source has no archive signature \
                 to check against it; refusing to install '{name}' unless it is already pinned \
                 by trust-on-first-use (see {PIN_FILE_NAME} in {})",
                install_dir.display()
            ));
        };
        existing
            .check(pin)
            .map_err(|e| format!("a signing key is configured; {e}"))
    }

    /// A content digest of a directory, used as its trust-on-first-use pin.
    ///
    /// Relative file names and contents are hashed in sorted order, so the pin
    /// changes if a file is added, removed or edited, and does not depend on
    /// the order the filesystem lists entries in.
    fn directory_digest(dir: &Path) -> Result<String, String> {
        fn collect(
            root: &Path,
            dir: &Path,
            out: &mut Vec<(String, PathBuf)>,
        ) -> Result<(), String> {
            let entries = std::fs::read_dir(dir)
                .map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
            for entry in entries.flatten() {
                let path = entry.path();
                let file_type = entry
                    .file_type()
                    .map_err(|e| format!("failed to stat {}: {e}", path.display()))?;
                if file_type.is_dir() {
                    collect(root, &path, out)?;
                } else if file_type.is_file() {
                    let relative = path
                        .strip_prefix(root)
                        .map_err(|_| format!("{} is outside {}", path.display(), root.display()))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((relative, path));
                }
            }
            Ok(())
        }

        let mut files = Vec::new();
        collect(dir, dir, &mut files)?;
        files.sort_by(|a, b| a.0.cmp(&b.0));

        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        for (relative, path) in files {
            let contents = std::fs::read(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            hasher.update(relative.as_bytes());
            hasher.update([0u8]);
            hasher.update((contents.len() as u64).to_le_bytes());
            hasher.update(&contents);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    /// Install a plugin from any supported source.
    ///
    /// A marketplace listing is not handled here: it carries a version and a
    /// download URL, so it goes through [`Self::install_plugin`]. Every other
    /// source stages into a temporary directory under `install_dir` and is
    /// moved into place with a single rename, so a failed install never leaves
    /// a half-written plugin behind.
    pub async fn install_from_source(
        &self,
        source: &PluginSource,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        match source {
            PluginSource::Marketplace { name } => Err(format!(
                "'{name}' is a marketplace listing; install it with install_plugin, which \
                 carries the listing's version and download URL"
            )),
            PluginSource::LocalDir(dir) => self.install_local_dir(dir, install_dir).await,
            PluginSource::Git { url, rev } => {
                self.install_git(url, rev.as_deref(), install_dir).await
            }
            PluginSource::Github { repo, rev, sha } => {
                let url = github_clone_url(repo)?;
                let rev = checkout_rev(rev.as_deref(), sha.as_deref())?;
                self.install_git(&url, rev, install_dir).await
            }
            PluginSource::GitSubdir {
                url,
                path,
                rev,
                sha,
            } => {
                self.install_git_subdir(url, path, rev.as_deref(), sha.as_deref(), install_dir)
                    .await
            }
            PluginSource::Archive { url, sha256 } => {
                self.install_archive(url, sha256.as_deref(), install_dir)
                    .await
            }
            PluginSource::Unsupported { kind } => Err(unsupported_source_reason(kind)),
        }
    }

    /// Install a plugin from a directory on this machine.
    ///
    /// The directory is copied, not moved or linked, so the source keeps
    /// working as a working copy. A directory has no archive and no commit, so
    /// there is nothing to pin: what protects it is that it is a path the user
    /// chose.
    pub async fn install_local_dir(
        &self,
        src: &Path,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        let manifest = Self::read_manifest_dir(src)?;
        self.check_install_compatible(&manifest)?;

        // A directory has no signature, so its pin is a digest of its contents:
        // that is what lets a configured key accept a directory only after a
        // trust-on-first-use install has recorded exactly this directory.
        // Decide before copying anything, so a refused install does no work.
        let pin = PluginPin {
            sha256: Some(Self::directory_digest(src)?),
            git_commit: None,
        };
        self.check_unverifiable_source(&manifest.name, "directory", install_dir, &pin)?;

        let staged = Self::staging_dir(install_dir, &manifest.name);
        let _ = std::fs::remove_dir_all(&staged);
        std::fs::create_dir_all(install_dir)
            .map_err(|e| format!("failed to create {}: {e}", install_dir.display()))?;
        if let Err(e) = copy_dir_recursive(src, &staged) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }

        self.finalize_install(&manifest.name, &staged, install_dir, &manifest, false, pin)
            .await
    }

    /// Install a plugin from a git repository.
    ///
    /// The clone is shallow unless `rev` is a full commit SHA, and the resolved
    /// commit becomes the plugin's pin: the next install of the same plugin has
    /// to come from that commit.
    pub async fn install_git(
        &self,
        url: &str,
        rev: Option<&str>,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        validate_git_url(url)?;

        let staged = Self::staging_dir(install_dir, url);
        let _ = std::fs::remove_dir_all(&staged);
        std::fs::create_dir_all(install_dir)
            .map_err(|e| format!("failed to create {}: {e}", install_dir.display()))?;

        let commit = match Self::clone_git(url, rev, &staged).await {
            Ok(commit) => commit,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }
        };

        let manifest = match Self::read_manifest_dir(&staged) {
            Ok(manifest) => manifest,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }
        };
        if let Err(e) = self.check_install_compatible(&manifest) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }

        let pin = PluginPin {
            sha256: None,
            git_commit: Some(commit),
        };
        if let Err(e) = self.check_unverifiable_source(&manifest.name, "git", install_dir, &pin) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }
        self.finalize_install(&manifest.name, &staged, install_dir, &manifest, false, pin)
            .await
    }

    /// Install a plugin from a subdirectory of a git repository.
    ///
    /// The repository is cloned with the git path's rules (transport validation,
    /// no credential prompt, no inherited `GIT_DIR`), the resolved commit is what
    /// the install is pinned to, and `path` is then resolved *inside* the clone:
    /// an absolute path, a `..` that climbs out, or a symlink that leaves the
    /// checkout is refused before anything is read or copied.
    pub async fn install_git_subdir(
        &self,
        url: &str,
        path: &str,
        rev: Option<&str>,
        sha: Option<&str>,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        validate_git_url(url)?;
        let rev = checkout_rev(rev, sha)?;

        let clone_dir = Self::staging_dir(install_dir, &format!("{url}#{path}"));
        let _ = std::fs::remove_dir_all(&clone_dir);
        std::fs::create_dir_all(install_dir)
            .map_err(|e| format!("failed to create {}: {e}", install_dir.display()))?;

        let commit = match Self::clone_git(url, rev, &clone_dir).await {
            Ok(commit) => commit,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&clone_dir);
                return Err(e);
            }
        };

        let subdir = match Self::resolve_subdir(&clone_dir, path) {
            Ok(subdir) => subdir,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&clone_dir);
                return Err(e);
            }
        };

        let manifest = match Self::read_manifest_dir(&subdir) {
            Ok(manifest) => manifest,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&clone_dir);
                return Err(e);
            }
        };
        if let Err(e) = self.check_install_compatible(&manifest) {
            let _ = std::fs::remove_dir_all(&clone_dir);
            return Err(e);
        }

        // The subdirectory is copied into its own staging directory, so
        // `finalize_install` sees exactly the same shape a git install produces
        // and the clone can be thrown away even if the move fails.
        let staged = Self::staging_dir(install_dir, &manifest.name);
        let _ = std::fs::remove_dir_all(&staged);
        if let Err(e) = copy_dir_recursive(&subdir, &staged) {
            let _ = std::fs::remove_dir_all(&staged);
            let _ = std::fs::remove_dir_all(&clone_dir);
            return Err(e);
        }
        let _ = std::fs::remove_dir_all(&clone_dir);

        let pin = PluginPin {
            sha256: None,
            git_commit: Some(commit),
        };
        if let Err(e) =
            self.check_unverifiable_source(&manifest.name, "git subdirectory", install_dir, &pin)
        {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }
        self.finalize_install(&manifest.name, &staged, install_dir, &manifest, false, pin)
            .await
    }

    /// Resolve a `git-subdir` path against a clone, refusing anything that
    /// leaves it.
    ///
    /// The name is checked twice: syntactically, so `..` and absolute paths are
    /// rejected for the reason they are dangerous, and afterwards by
    /// canonicalising, so a symlink pointing out of the checkout is caught too.
    fn resolve_subdir(clone_root: &Path, path: &str) -> Result<PathBuf, String> {
        if path.trim().is_empty() {
            return Err("git-subdir source has an empty path".to_string());
        }
        for component in Path::new(path).components() {
            match component {
                std::path::Component::ParentDir => {
                    return Err(format!(
                        "refusing git-subdir path '{path}': it climbs out of the repository"
                    ));
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    return Err(format!("refusing git-subdir path '{path}': it is absolute"));
                }
                _ => {}
            }
        }

        let root = clone_root
            .canonicalize()
            .map_err(|e| format!("failed to resolve {}: {e}", clone_root.display()))?;
        let resolved = clone_root.join(path).canonicalize().map_err(|_| {
            format!("git-subdir path '{path}' is not a directory in the repository")
        })?;
        if !resolved.starts_with(&root) || !resolved.is_dir() {
            return Err(format!(
                "refusing git-subdir path '{path}': it does not resolve inside the repository"
            ));
        }
        Ok(resolved)
    }

    /// Clone a repository into `dest` and report the commit that landed there.
    async fn clone_git(url: &str, rev: Option<&str>, dest: &Path) -> Result<String, String> {
        let dest_arg = dest.to_string_lossy().to_string();
        let commit_rev = rev.filter(|r| is_full_commit_sha(r));

        let mut args: Vec<String> = vec!["clone".into()];
        if commit_rev.is_some() {
            // `--branch` takes a ref, not a SHA, so pinning to a commit means
            // taking the history and checking it out.
            args.push("--no-checkout".into());
        } else {
            args.push("--depth".into());
            args.push("1".into());
            if let Some(rev) = rev {
                args.push("--branch".into());
                args.push(rev.to_string());
            }
        }
        args.push("--".into());
        args.push(url.to_string());
        args.push(dest_arg.clone());

        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(&refs).await?;

        if let Some(sha) = commit_rev {
            run_git(&["-C", dest_arg.as_str(), "checkout", "--detach", sha]).await?;
        }

        let commit = run_git(&["-C", dest_arg.as_str(), "rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string();

        if let Some(sha) = commit_rev {
            if !commit.eq_ignore_ascii_case(sha) {
                return Err(format!(
                    "git checkout is at {commit}, not the requested commit {sha}"
                ));
            }
        }

        // The checkout is a snapshot now. Dropping `.git` keeps the installed
        // plugin from looking like a repository to the project around it.
        let _ = std::fs::remove_dir_all(dest.join(".git"));

        Ok(commit)
    }

    /// Install a plugin from an HTTPS archive.
    ///
    /// With a signing key configured, the detached signature is fetched from
    /// `<url>.sig` (hex-encoded) and checked against exactly the downloaded
    /// bytes. The manifest's own `signature` field is not used: the manifest is
    /// inside the archive, so it cannot sign that archive. The archive's
    /// SHA-256 is recorded as a pin.
    pub async fn install_archive(
        &self,
        url: &str,
        sha256: Option<&str>,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        if !url.starts_with("https://") {
            return Err(format!(
                "refusing to install plugin from non-HTTPS URL '{url}'"
            ));
        }

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        let bytes = match client.get(url).send().await {
            Ok(response) if response.status().is_success() => match response.bytes().await {
                Ok(bytes) => bytes.to_vec(),
                Err(e) => return Err(format!("failed to read plugin archive: {e}")),
            },
            Ok(response) => return Err(format!("download failed: HTTP {}", response.status())),
            Err(e) => return Err(format!("failed to download plugin archive: {e}")),
        };

        if let Some(expected) = sha256 {
            let digest = sha256_hex(&bytes);
            if !expected.eq_ignore_ascii_case(&digest) {
                return Err(format!(
                    "plugin archive does not match the requested SHA-256 (expected {expected}, \
                     got {digest})"
                ));
            }
        }

        // Only fetch the sidecar when there is a key to check it with: with no
        // key the archive is installed unverified, exactly as before.
        let signature = if self.public_key.is_some() {
            Some(Self::fetch_detached_signature(&client, url).await?)
        } else {
            None
        };

        self.install_archive_contents(&bytes, signature.as_deref(), install_dir)
            .await
    }

    /// Install an archive the caller has already fetched.
    ///
    /// `detached_signature_hex` is a signature over exactly `bytes`, supplied
    /// by the caller instead of read from a sidecar. With a public key
    /// configured it is mandatory; without one it is ignored and the install is
    /// reported as unverified.
    pub async fn install_archive_bytes(
        &self,
        bytes: &[u8],
        detached_signature_hex: Option<&str>,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        self.install_archive_contents(bytes, detached_signature_hex, install_dir)
            .await
    }

    /// Fetch the detached signature that sits beside an archive.
    async fn fetch_detached_signature(
        client: &Client,
        archive_url: &str,
    ) -> Result<String, String> {
        let signature_url = format!("{archive_url}.sig");
        let response = client
            .get(&signature_url)
            .send()
            .await
            .map_err(|e| format!("failed to download signature '{signature_url}': {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "plugin signature missing: '{signature_url}' returned HTTP {} but a signing key \
                 is configured",
                response.status()
            ));
        }
        let text = response
            .text()
            .await
            .map_err(|e| format!("failed to read signature '{signature_url}': {e}"))?;
        let text = text.trim();
        if text.is_empty() {
            return Err(format!(
                "plugin signature missing: '{signature_url}' is empty but a signing key is \
                 configured"
            ));
        }
        Ok(text.to_string())
    }

    /// Extract, verify and install archive bytes.
    async fn install_archive_contents(
        &self,
        bytes: &[u8],
        detached_signature_hex: Option<&str>,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        let digest = sha256_hex(bytes);
        let staged = Self::staging_dir(
            install_dir,
            &format!("archive-{}", &digest[..16.min(digest.len())]),
        );
        let _ = std::fs::remove_dir_all(&staged);
        std::fs::create_dir_all(&staged).map_err(|e| format!("failed to create temp dir: {e}"))?;

        if let Err(e) = Self::extract_archive(bytes, &staged) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }

        let manifest = match Self::read_manifest_dir(&staged) {
            Ok(manifest) => manifest,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }
        };
        if let Err(e) = self.check_install_compatible(&manifest) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e);
        }

        // The signature is detached: it covers the archive bytes, not a field
        // inside the archive (which could never be self-referential anyway).
        let verified = match self.verify_signature(bytes, detached_signature_hex) {
            Ok(check) => matches!(check, SignatureCheck::Verified),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staged);
                return Err(e);
            }
        };
        if !verified {
            tracing::warn!(
                plugin = %manifest.name,
                "installing unsigned plugin: no signing key is configured, so nothing about \
                 this archive has been checked"
            );
        }

        self.finalize_install(
            &manifest.name,
            &staged,
            install_dir,
            &manifest,
            verified,
            PluginPin {
                sha256: Some(digest),
                git_commit: None,
            },
        )
        .await
    }

    /// Take the install lock, enforce the pin, move the staged plugin into
    /// place, and record what was installed.
    async fn finalize_install(
        &self,
        name: &str,
        staged: &Path,
        install_dir: &Path,
        manifest: &PluginManifest,
        verified: bool,
        pin: PluginPin,
    ) -> Result<PathBuf, String> {
        let lock = {
            let arc = self
                .install_locks
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone();
            arc
        };
        let _guard = lock.lock().await;

        if let Err(e) = Self::validate_plugin_name(name) {
            let _ = std::fs::remove_dir_all(staged);
            return Err(e);
        }

        let target_dir = install_dir.join(name);
        if target_dir.exists() {
            let _ = std::fs::remove_dir_all(staged);
            return Err(format!(
                "plugin '{name}' is already installed at {}",
                target_dir.display()
            ));
        }

        // Trust on first use: the first install records what it got, and later
        // installs have to match it.
        let store = match Self::check_pin(install_dir, name, &pin) {
            Ok(store) => store,
            Err(e) => {
                let _ = std::fs::remove_dir_all(staged);
                self.set_plugin_state(name, PluginState::Error(e.clone()));
                return Err(e);
            }
        };

        if let Err(e) = std::fs::rename(staged, &target_dir) {
            let _ = std::fs::remove_dir_all(staged);
            return Err(format!("failed to finalize plugin install: {e}"));
        }

        Self::record_pin(install_dir, store, name, &pin);
        manifest.warn_unimplemented_fields();

        let handle = PluginHandle {
            name: name.to_string(),
            version: manifest.version.clone(),
            state: PluginState::Active,
            path: target_dir.clone(),
            commands: manifest.commands.clone(),
            skills: manifest.skills.clone(),
            retry_count: 0,
            max_retries: 3,
            verified,
            last_retry_at: None,
        };
        self.plugins.insert(name.to_string(), handle);

        tracing::info!(
            plugin = %name,
            version = %manifest.version,
            path = %target_dir.display(),
            "installed plugin"
        );
        self.mark_needs_refresh();

        Ok(target_dir)
    }

    /// Load the pin store and check a candidate pin against it.
    fn check_pin(
        install_dir: &Path,
        name: &str,
        pin: &PluginPin,
    ) -> Result<PluginPinStore, String> {
        let store = PluginPinStore::load(&install_dir.join(PIN_FILE_NAME))?;
        if let Some(existing) = store.get(name) {
            existing.check(pin)?;
        }
        Ok(store)
    }

    /// Record a pin and write the store back.
    ///
    /// A pin that cannot be written does not fail the install — the plugin is
    /// already in place and correct — but it is said out loud, because the
    /// protection it would have given is now missing.
    fn record_pin(install_dir: &Path, mut store: PluginPinStore, name: &str, pin: &PluginPin) {
        if pin.is_empty() || store.get(name) == Some(pin) {
            return;
        }
        store.plugins.insert(name.to_string(), pin.clone());
        let path = install_dir.join(PIN_FILE_NAME);
        match store.save(&path) {
            Ok(()) => tracing::info!(plugin = %name, "pinned plugin contents for future installs"),
            Err(e) => tracing::warn!(
                plugin = %name,
                "installed, but the pin could not be written to {}: {e}",
                path.display()
            ),
        }
    }

    /// Install a plugin from the marketplace.
    ///
    /// Uses atomic temp-dir staging: downloads and extracts into a temporary
    /// directory, validates the structure, then renames atomically to the final
    /// location. On failure, the temporary directory is removed.
    ///
    /// Two forms are accepted:
    /// - a real [`MarketplacePlugin`] listing, whose `url` is its download URL;
    /// - a listing whose `url` is a bare plugin name, which is looked up in the
    ///   registered marketplaces. [`Self::install_plugin_by_name`] is the typed
    ///   entry point for that form.
    ///
    /// Whichever form is used, the archive's detached signature is fetched
    /// from `<download-url>.sig` when a signing key is configured.
    pub async fn install_plugin(
        &self,
        plugin: &MarketplacePlugin,
        install_dir: &PathBuf,
    ) -> Result<PathBuf, String> {
        // `coderex plugin install <source>` passes its argument through both
        // the name and the url. When that argument names a directory, a git
        // remote or an archive, install from there rather than looking it up in
        // a marketplace. A real listing's url never looks like either.
        let detected = PluginSource::detect(&plugin.url);
        if !detected.is_marketplace() {
            return self.install_from_source(&detected, install_dir).await;
        }

        // A bare string is a marketplace plugin name; anything with a scheme is
        // the download URL of an already-fetched listing.
        let marketplace_name = match &detected {
            PluginSource::Marketplace { name } if !name.contains("://") => Some(name.clone()),
            _ => None,
        };
        let plugin_name = marketplace_name
            .clone()
            .unwrap_or_else(|| plugin.name.clone());

        if marketplace_name.is_some() && self.marketplaces.is_empty() {
            return Err(format!(
                "'{plugin_name}' is a marketplace plugin name, but no marketplace is \
                 registered; call PluginManager::register_marketplace(name, base_url) first"
            ));
        }

        Self::validate_plugin_name(&plugin_name)?;
        let target_dir = install_dir.join(&plugin_name);

        // Acquire install lock for this plugin name
        let lock = {
            let arc = self
                .install_locks
                .entry(plugin_name.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone();
            arc
        };
        let _guard = lock.lock().await;

        // Check if already installed
        if target_dir.exists() {
            return Err(format!(
                "plugin '{plugin_name}' is already installed at {target_dir:?}"
            ));
        }

        // Mark as installing
        let handle = PluginHandle {
            name: plugin_name.clone(),
            version: plugin.version.clone(),
            state: PluginState::Installing,
            path: target_dir.clone(),
            commands: Vec::new(),
            skills: Vec::new(),
            retry_count: 0,
            max_retries: 3,
            verified: false,
            last_retry_at: None,
        };
        self.plugins.insert(plugin_name.clone(), handle);

        // Create a temporary staging directory
        let temp_dir = install_dir.join(format!(".install-{plugin_name}"));
        let _ = std::fs::remove_dir_all(&temp_dir); // clean up any previous failed attempt

        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            self.set_plugin_state(
                &plugin_name,
                PluginState::Error(format!("temp dir creation failed: {e}")),
            );
            return Err(format!("failed to create temp dir: {e}"));
        }

        // Download the plugin archive. Keep the URL and client that served it
        // so the detached signature can be fetched from beside the archive.
        let archive = 'download: {
            for entry in self.marketplaces.iter() {
                let conn = entry.value();
                let download_path = conn.download_endpoint.replace("{name}", &plugin_name);
                let url = format!("{}{}", conn.base_url, download_path);

                match conn.client.get(&url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            match response.bytes().await {
                                Ok(bytes) => {
                                    break 'download Some((
                                        bytes.to_vec(),
                                        url,
                                        conn.client.clone(),
                                    ));
                                }
                                Err(e) => tracing::warn!("failed to read plugin archive: {e}"),
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to download plugin '{plugin_name}': {e}");
                    }
                }
            }
            None
        };

        let (bytes, download_url, client) = match archive {
            Some(found) => found,
            None => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                self.set_plugin_state(
                    &plugin_name,
                    PluginState::Error("download failed from all marketplaces".into()),
                );
                return Err(format!(
                    "failed to download plugin '{plugin_name}' from any marketplace"
                ));
            }
        };

        // Extract the archive into the temp directory
        if let Err(e) = Self::extract_archive(&bytes, &temp_dir) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            self.set_plugin_state(
                &plugin_name,
                PluginState::Error(format!("extraction failed: {e}")),
            );
            return Err(e);
        }

        // Validate extracted structure
        let manifest = match Self::validate_extracted_plugin(&temp_dir) {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                self.set_plugin_state(
                    &plugin_name,
                    PluginState::Error(format!("validation failed: {e}")),
                );
                return Err(e);
            }
        };

        // Version compatibility check
        if !self.is_version_compatible(manifest.min_version.as_deref()) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            let msg = format!(
                "plugin '{plugin_name}' requires CodeRex >= {} (current: {})",
                manifest.min_version.as_deref().unwrap_or("unknown"),
                self.app_version
            );
            self.set_plugin_state(&plugin_name, PluginState::Error(msg.clone()));
            return Err(msg);
        }

        // Signature verification. The signature is detached (`<url>.sig`); a
        // signature inside the manifest cannot cover the archive it lives in.
        // With a configured key the detached signature is mandatory.
        let detached_signature = if self.public_key.is_some() {
            match Self::fetch_detached_signature(&client, &download_url).await {
                Ok(signature) => Some(signature),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&temp_dir);
                    self.set_plugin_state(
                        &plugin_name,
                        PluginState::Error(format!("signature verification failed: {e}")),
                    );
                    return Err(e);
                }
            }
        } else {
            None
        };
        let signature = match self.verify_signature(&bytes, detached_signature.as_deref()) {
            Ok(check) => check,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                self.set_plugin_state(
                    &plugin_name,
                    PluginState::Error(format!("signature verification failed: {e}")),
                );
                return Err(e);
            }
        };
        let verified = matches!(signature, SignatureCheck::Verified);
        if !verified {
            tracing::warn!(
                plugin = %plugin_name,
                "installing unsigned plugin: no signing key is configured, so nothing about \
                 this archive has been checked"
            );
        }

        // Pin the archive's contents before anything is moved into place, so a
        // marketplace that serves different bytes the second time is refused
        // rather than installed.
        let pin = PluginPin {
            sha256: Some(sha256_hex(&bytes)),
            git_commit: None,
        };
        let store = match Self::check_pin(install_dir, &plugin_name, &pin) {
            Ok(store) => store,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                self.set_plugin_state(&plugin_name, PluginState::Error(e.clone()));
                return Err(e);
            }
        };

        // Atomically rename temp dir to final location
        if let Err(e) = std::fs::rename(&temp_dir, &target_dir) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            self.set_plugin_state(
                &plugin_name,
                PluginState::Error(format!("atomic rename failed: {e}")),
            );
            return Err(format!("failed to finalize plugin install: {e}"));
        }

        Self::record_pin(install_dir, store, &plugin_name, &pin);

        // Clean up the archive file if it was left in the target dir
        let archive_path = target_dir.join("plugin.tar.gz");
        if archive_path.exists() {
            let _ = std::fs::remove_file(&archive_path);
        }

        // Update state to active
        let handle = PluginHandle {
            name: plugin_name.clone(),
            version: plugin.version.clone(),
            state: PluginState::Active,
            path: target_dir.clone(),
            commands: manifest.commands,
            skills: manifest.skills,
            retry_count: 0,
            max_retries: 3,
            verified,
            last_retry_at: None,
        };
        self.plugins.insert(plugin_name.clone(), handle);

        tracing::info!("installed plugin '{plugin_name}' v{}", plugin.version);

        self.mark_needs_refresh();
        Ok(target_dir)
    }

    /// Install a plugin by its name in a registered marketplace.
    ///
    /// This is the production entry point for `coderex plugin install <name>`:
    /// register the marketplace once (see [`Self::register_marketplace`]), then
    /// call this with the name the marketplace lists. It fails with a clear
    /// error if no marketplace is registered, instead of silently reporting a
    /// download failure.
    pub async fn install_plugin_by_name(
        &self,
        name: &str,
        install_dir: &Path,
    ) -> Result<PathBuf, String> {
        let listing = MarketplacePlugin {
            name: name.to_string(),
            version: "latest".to_string(),
            // A bare name is what install_plugin resolves against the
            // registered marketplaces.
            url: name.to_string(),
            description: String::new(),
        };
        self.install_plugin(&listing, &install_dir.to_path_buf())
            .await
    }

    /// Set the state of a plugin by name.
    fn set_plugin_state(&self, name: &str, state: PluginState) {
        if let Some(mut entry) = self.plugins.get_mut(name) {
            entry.state = state;
        }
    }

    /// Spawn the background loop that drives [`Self::retry_failed_plugins`].
    ///
    /// The loop holds a `Weak` reference, so it does not keep the manager
    /// alive: when the session drops its last `Arc`, the next tick cannot
    /// upgrade and the task exits. The caller may detach the returned handle;
    /// it is exposed so a test can abort the loop.
    ///
    /// `interval` is how often the backoff is *checked*, not the backoff
    /// itself — [`Self::retry_failed_plugins`] decides whether enough time has
    /// passed for a particular plugin.
    #[must_use]
    pub fn spawn_retry_loop(
        self: &Arc<Self>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                for outcome in manager.retry_failed_plugins() {
                    if let RetryOutcome::RestartRequired(name) = outcome {
                        tracing::warn!(
                            plugin = %name,
                            "plugin recovered but no restart handler is registered; call \
                             spawn_plugin to bring it back"
                        );
                    }
                }
            }
        })
    }

    /// Retry loading plugins that are in the `Error` state.
    ///
    /// Uses exponential backoff: waits `2^retry_count` seconds before retrying.
    /// Plugins that exceed `max_retries` are left in Error state.
    ///
    /// A retry that can read the manifest again is not enough to call the
    /// plugin loaded: the runtime may never have been spawned. If a restart
    /// handler was registered with [`Self::set_restart_handler`], it is invoked
    /// and the plugin becomes `Active` only when it returns `Ok`. Without a
    /// handler the plugin becomes `Loading` and the outcome is
    /// [`RetryOutcome::RestartRequired`], so the caller knows to spawn it.
    pub fn retry_failed_plugins(&self) -> Vec<RetryOutcome> {
        let mut outcomes = Vec::new();

        for mut entry in self.plugins.iter_mut() {
            let plugin = entry.value_mut();
            if !matches!(plugin.state, PluginState::Error(_)) {
                continue;
            }
            if plugin.retry_count >= plugin.max_retries {
                continue;
            }

            // Back off between attempts, as the retry policy says it does: a
            // plugin whose file is still being written recovers on the next
            // attempt, and one that is broken does not get hammered.
            if let Some(last) = plugin.last_retry_at {
                let waited = chrono::Utc::now()
                    .signed_duration_since(last)
                    .to_std()
                    .unwrap_or_default();
                if waited < retry_backoff(plugin.retry_count) {
                    continue;
                }
            }

            plugin.retry_count += 1;
            plugin.last_retry_at = Some(chrono::Utc::now());
            let name = plugin.name.clone();

            // Attempt to re-read the manifest
            let manifest_path = plugin.path.join("plugin.json");
            if !manifest_path.exists() {
                plugin.state = PluginState::Error(format!(
                    "manifest missing (retry {}/{})",
                    plugin.retry_count, plugin.max_retries
                ));
                outcomes.push(RetryOutcome::StillFailed(name));
                continue;
            }

            match std::fs::read_to_string(&manifest_path) {
                Ok(content) => match serde_json::from_str::<PluginManifest>(&content) {
                    Ok(manifest) => {
                        if !self.is_version_compatible(manifest.min_version.as_deref()) {
                            plugin.state = PluginState::Error(format!(
                                "incompatible version (retry {}/{})",
                                plugin.retry_count, plugin.max_retries
                            ));
                            outcomes.push(RetryOutcome::StillFailed(name));
                            continue;
                        }

                        plugin.version = manifest.version;
                        plugin.commands = manifest.commands;
                        plugin.skills = manifest.skills;

                        if let Some(handler) = &self.restart_handler {
                            match handler.call(&name) {
                                Ok(()) => {
                                    plugin.state = PluginState::Active;
                                    plugin.retry_count = 0;
                                    tracing::info!(
                                        plugin = %name,
                                        "plugin recovered from error state and was restarted"
                                    );
                                    outcomes.push(RetryOutcome::Restarted(name));
                                }
                                Err(e) => {
                                    plugin.state = PluginState::Error(format!(
                                        "restart failed after recovery: {e} (retry {}/{})",
                                        plugin.retry_count, plugin.max_retries
                                    ));
                                    outcomes.push(RetryOutcome::StillFailed(name));
                                }
                            }
                        } else {
                            // The manifest is readable again, but nothing has
                            // spawned the runtime: say so instead of pretending
                            // the plugin is active.
                            plugin.state = PluginState::Loading;
                            tracing::info!(
                                plugin = %name,
                                "plugin manifest recovered; waiting for the caller to spawn it"
                            );
                            outcomes.push(RetryOutcome::RestartRequired(name));
                        }
                    }
                    Err(e) => {
                        plugin.state = PluginState::Error(format!(
                            "manifest parse failed: {e} (retry {}/{})",
                            plugin.retry_count, plugin.max_retries
                        ));
                        outcomes.push(RetryOutcome::StillFailed(name));
                    }
                },
                Err(e) => {
                    plugin.state = PluginState::Error(format!(
                        "manifest read failed: {e} (retry {}/{})",
                        plugin.retry_count, plugin.max_retries
                    ));
                    outcomes.push(RetryOutcome::StillFailed(name));
                }
            }
        }

        outcomes
    }

    /// Reconcile marketplace state — detect new, updated, or removed plugins.
    pub async fn reconcile(&self) -> Result<ReconciliationReport, String> {
        let local_plugins = self.discover();
        let local_names: std::collections::HashSet<_> =
            local_plugins.iter().map(|p| &p.name).collect();

        let marketplace_plugins = self.fetch_marketplace_plugins().await?;
        let marketplace_names: std::collections::HashSet<_> =
            marketplace_plugins.iter().map(|p| &p.name).collect();

        // New plugins (in marketplace but not locally)
        let new_plugins: Vec<&MarketplacePlugin> = marketplace_plugins
            .iter()
            .filter(|p| !local_names.contains(&p.name))
            .collect();

        // Updated plugins (different versions)
        let updated_plugins: Vec<(&MarketplacePlugin, &PluginManifest)> = marketplace_plugins
            .iter()
            .filter_map(|mp| {
                local_plugins
                    .iter()
                    .find(|lp| lp.name == mp.name)
                    .map(|lp| {
                        if mp.version != lp.version {
                            (mp, lp)
                        } else {
                            (mp, lp) // Include even if same version for completeness
                        }
                    })
            })
            .filter(|(mp, lp)| mp.version != lp.version)
            .collect();

        // Removed plugins (locally present but not in marketplace)
        let removed_plugins: Vec<&PluginManifest> = local_plugins
            .iter()
            .filter(|p| !marketplace_names.contains(&p.name))
            .collect();

        let report = ReconciliationReport {
            new_count: new_plugins.len(),
            updated_count: updated_plugins.len(),
            removed_count: removed_plugins.len(),
            new_plugins: new_plugins.into_iter().cloned().collect(),
            updated_plugins: updated_plugins
                .into_iter()
                .map(|(mp, lp)| (mp.clone(), lp.clone()))
                .collect(),
            removed_plugins: removed_plugins.into_iter().cloned().collect(),
        };

        if !report.is_empty() {
            tracing::info!(
                "marketplace reconciliation: {} new, {} updated, {} removed",
                report.new_count,
                report.updated_count,
                report.removed_count
            );
        }

        Ok(report)
    }

    /// Auto-install new plugins from marketplace.
    pub async fn auto_install_new(&self, install_dir: &PathBuf) -> Vec<Result<PathBuf, String>> {
        let report = match self.reconcile().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("reconciliation failed: {e}");
                return Vec::new();
            }
        };

        let mut results = Vec::new();
        for plugin in &report.new_plugins {
            let result = self.install_plugin(plugin, install_dir).await;
            results.push(result);
        }

        if !results.is_empty() {
            self.mark_needs_refresh();
        }

        results
    }

    /// Get all commands registered across all plugins.
    #[must_use]
    pub fn all_commands(&self) -> Vec<String> {
        self.plugins
            .iter()
            .flat_map(|e| e.value().commands.clone())
            .collect()
    }

    /// Get all skills registered across all plugins.
    #[must_use]
    pub fn all_skills(&self) -> Vec<String> {
        self.plugins
            .iter()
            .flat_map(|e| e.value().skills.clone())
            .collect()
    }

    /// Get a plugin's installed path.
    #[must_use]
    pub fn get_plugin_path(&self, name: &str) -> Option<PathBuf> {
        self.plugins.get(name).map(|e| e.value().path.clone())
    }

    /// Set the Ed25519 public key for signature verification (hex-encoded).
    ///
    /// This is the trust anchor: it has to come from whoever runs CodeRex
    /// (see [`Self::set_public_key_from_env`]), never from the package being
    /// checked. With a key set, an archive that does not match it is refused.
    pub fn set_public_key(&mut self, key_hex: impl Into<String>) {
        self.public_key = Some(key_hex.into());
    }

    /// Take the signing key from the environment, if it is set.
    ///
    /// The environment is the one source a repository cannot write, which is
    /// what makes it a usable trust anchor. Returns whether a key was found.
    pub fn set_public_key_from_env(&mut self) -> bool {
        self.apply_public_key(std::env::var(PLUGIN_PUBLIC_KEY_ENV).ok())
    }

    /// Take the signing key from a raw configuration value.
    ///
    /// An unset or blank value leaves any existing key alone and reports that
    /// nothing was configured.
    fn apply_public_key(&mut self, key: Option<String>) -> bool {
        match key {
            Some(key) if !key.trim().is_empty() => {
                self.set_public_key(key.trim().to_string());
                true
            }
            _ => false,
        }
    }

    /// Verify a plugin archive signature using the configured public key.
    ///
    /// Verified means a configured key matched the archive. Unverified means
    /// there was no key to check against — the caller has to decide whether an
    /// unchecked plugin is acceptable; this never reports a check that did not
    /// happen. `Err` is a plugin that had to be checked and was not, or did not
    /// match: with a key configured, a missing or wrong signature is fatal.
    pub fn verify_signature(
        &self,
        archive_bytes: &[u8],
        signature_hex: Option<&str>,
    ) -> Result<SignatureCheck, String> {
        let Some(key_hex) = &self.public_key else {
            return Ok(SignatureCheck::Unverified);
        };

        let Some(sig_hex) = signature_hex else {
            return Err("plugin signature missing but public key is configured".into());
        };

        let key_bytes = hex::decode(key_hex).map_err(|e| format!("invalid public key hex: {e}"))?;
        if key_bytes.len() != 32 {
            return Err(format!(
                "invalid public key length: expected 32, got {}",
                key_bytes.len()
            ));
        }

        let sig_bytes = hex::decode(sig_hex).map_err(|e| format!("invalid signature hex: {e}"))?;
        if sig_bytes.len() != 64 {
            return Err(format!(
                "invalid signature length: expected 64, got {}",
                sig_bytes.len()
            ));
        }

        let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(
            &key_bytes
                .try_into()
                .map_err(|_| "public key conversion failed")?,
        )
        .map_err(|e| format!("invalid Ed25519 public key: {e}"))?;

        let signature = ed25519_dalek::Signature::from_bytes(
            &sig_bytes
                .try_into()
                .map_err(|_| "signature conversion failed")?,
        );

        verifying_key
            .verify(archive_bytes, &signature)
            .map_err(|e| format!("signature verification failed: {e}"))?;

        Ok(SignatureCheck::Verified)
    }

    /// Resolve plugin dependencies and return load order.
    ///
    /// Performs a topological sort: plugins with no dependencies first,
    /// then plugins whose dependencies are all satisfied.
    ///
    /// Returns `Err` if a dependency cycle is detected or a required
    /// dependency is missing.
    pub fn resolve_load_order(&self) -> Result<Vec<String>, String> {
        let mut in_degrees: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut adjacency: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut plugin_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Build graph
        for entry in self.plugins.iter() {
            let plugin = entry.value();
            let name = plugin.name.clone();
            plugin_names.insert(name.clone());
            in_degrees.entry(name.clone()).or_insert(0);

            // PluginHandle doesn't store dependencies; we read from manifest below
            let _ = plugin.skills;
        }

        // Re-read manifests for dependency info
        for entry in self.plugins.iter() {
            let plugin = entry.value();
            let manifest_path = plugin.path.join("plugin.json");
            let deps = if manifest_path.exists() {
                std::fs::read_to_string(&manifest_path)
                    .ok()
                    .and_then(|c| serde_json::from_str::<PluginManifest>(&c).ok())
                    .map(|m| m.dependencies)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            for dep in deps {
                adjacency
                    .entry(dep.clone())
                    .or_default()
                    .push(plugin.name.clone());
                *in_degrees.entry(plugin.name.clone()).or_insert(0) += 1;
                if !plugin_names.contains(&dep) {
                    return Err(format!(
                        "plugin '{}' depends on '{}' which is not installed",
                        plugin.name, dep
                    ));
                }
            }
        }

        // Kahn's algorithm
        let mut queue: Vec<String> = in_degrees
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(name, _)| name.clone())
            .collect();
        let mut order = Vec::new();

        while let Some(name) = queue.pop() {
            order.push(name.clone());
            if let Some(children) = adjacency.get(&name) {
                for child in children {
                    if let Some(deg) = in_degrees.get_mut(child) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push(child.clone());
                        }
                    }
                }
            }
        }

        if order.len() != plugin_names.len() {
            return Err("dependency cycle detected among plugins".into());
        }

        Ok(order)
    }
}

/// Report of marketplace reconciliation.
#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    /// Number of new plugins available.
    pub new_count: usize,
    /// Number of plugins with updates available.
    pub updated_count: usize,
    /// Number of plugins no longer in marketplace.
    pub removed_count: usize,
    /// New plugin listings.
    pub new_plugins: Vec<MarketplacePlugin>,
    /// Updated plugins (marketplace version, local version).
    pub updated_plugins: Vec<(MarketplacePlugin, PluginManifest)>,
    /// Removed plugin manifests.
    pub removed_plugins: Vec<PluginManifest>,
}

impl ReconciliationReport {
    /// Check if the report has any changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.new_count == 0 && self.updated_count == 0 && self.removed_count == 0
    }
}

/// Perform background marketplace reconciliation.
#[derive(Debug)]
pub struct MarketplaceReconciler {
    /// How often to reconcile (in seconds).
    pub interval_secs: u64,
    /// Whether auto-install is enabled.
    pub auto_install: bool,
}

impl MarketplaceReconciler {
    /// Create a new reconciler.
    #[must_use]
    pub fn new(interval_secs: u64) -> Self {
        Self {
            interval_secs,
            auto_install: false,
        }
    }

    /// Create a new reconciler with auto-install enabled.
    #[must_use]
    pub fn with_auto_install(interval_secs: u64) -> Self {
        Self {
            interval_secs,
            auto_install: true,
        }
    }

    /// Run one reconciliation cycle.
    pub async fn reconcile(
        &self,
        manager: &PluginManager,
        install_dir: Option<&PathBuf>,
    ) -> Result<ReconciliationReport, String> {
        let report = manager.reconcile().await?;

        if self.auto_install {
            if let Some(dir) = install_dir {
                let results = manager.auto_install_new(dir).await;
                let installed = results.iter().filter(|r| r.is_ok()).count();
                let failed = results.iter().filter(|r| r.is_err()).count();

                if installed > 0 {
                    tracing::info!("auto-installed {} plugins, {} failed", installed, failed);
                }
            }
        }

        Ok(report)
    }
}

/// How long to wait before the next recovery attempt.
///
/// Exponential from a second and capped at a minute: long enough that a
/// half-written plugin file has settled, short enough that a plugin which
/// becomes usable is picked up without a restart.
#[must_use]
fn retry_backoff(retry_count: u32) -> std::time::Duration {
    let seconds = 1u64 << retry_count.min(6);
    std::time::Duration::from_secs(seconds.min(60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_manager_is_empty() {
        let mgr = PluginManager::new();
        assert!(mgr.list_plugins().is_empty());
        assert!(!mgr.needs_refresh());
    }

    #[test]
    fn load_and_list_plugin() {
        let mgr = PluginManager::new();
        let manifest = PluginManifest {
            name: "test-plugin".into(),
            version: "1.0.0".into(),
            description: "Test".into(),
            author: "test".into(),
            min_version: None,
            commands: vec!["cmd1".into()],
            skills: vec!["skill1".into()],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        mgr.load_plugin(manifest, PathBuf::from("/plugins/test"));
        assert_eq!(mgr.len(), 1);
        assert_eq!(mgr.list_plugins(), vec!["test-plugin"]);

        let plugin = mgr.get_plugin("test-plugin").unwrap();
        assert_eq!(plugin.commands, vec!["cmd1"]);
        assert_eq!(plugin.skills, vec!["skill1"]);
        // A plugin loaded from a directory has no archive to check.
        assert!(!plugin.verified);
    }

    #[test]
    fn unload_plugin() {
        let mgr = PluginManager::new();
        let manifest = PluginManifest {
            name: "temp".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        mgr.load_plugin(manifest, PathBuf::from("/tmp"));
        assert!(mgr.unload_plugin("temp"));
        assert!(mgr.get_plugin("temp").is_none());
    }

    #[test]
    fn unload_nonexistent_returns_false() {
        let mgr = PluginManager::new();
        assert!(!mgr.unload_plugin("nope"));
    }

    #[test]
    fn refresh_flag() {
        let mgr = PluginManager::new();
        assert!(!mgr.needs_refresh());
        mgr.mark_needs_refresh();
        assert!(mgr.needs_refresh());
        mgr.clear_refresh_flag();
        assert!(!mgr.needs_refresh());
    }

    #[test]
    fn all_commands_and_skills() {
        let mgr = PluginManager::new();
        let m1 = PluginManifest {
            name: "a".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec!["cmd1".into()],
            skills: vec!["sk1".into()],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        let m2 = PluginManifest {
            name: "b".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec!["cmd2".into()],
            skills: vec!["sk2".into()],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        mgr.load_plugin(m1, PathBuf::new());
        mgr.load_plugin(m2, PathBuf::new());
        assert_eq!(mgr.all_commands().len(), 2);
        assert_eq!(mgr.all_skills().len(), 2);
    }

    #[test]
    fn marketplace_registration() {
        let mgr = PluginManager::new();
        let result =
            mgr.register_marketplace("main".into(), "https://marketplace.example.com".into());
        assert!(result.is_ok(), "marketplace registration should succeed");
    }

    #[test]
    fn discover_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("test-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = PluginManifest {
            name: "test-plugin".into(),
            version: "0.1.0".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        std::fs::write(
            plugin_dir.join("plugin.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let mut mgr = PluginManager::new();
        mgr.add_plugin_dir(dir.path().to_path_buf());
        let found = mgr.discover();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "test-plugin");
    }

    #[tokio::test]
    async fn marketplace_reconciler_no_connectivity() {
        let reconciler = MarketplaceReconciler::new(300);
        assert_eq!(reconciler.interval_secs, 300);
        let mgr = PluginManager::new();
        // Without a registered marketplace, reconcile returns empty report
        let result = reconciler.reconcile(&mgr, None).await;
        assert!(result.is_ok());
        let report = result.unwrap();
        assert!(report.is_empty());
    }

    #[tokio::test]
    async fn fetch_marketplace_no_connectivity() {
        let mgr = PluginManager::new();
        // Without a registered marketplace, returns empty vec
        let result = mgr.fetch_marketplace_plugins().await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn auto_install_no_connectivity() {
        let mgr = PluginManager::new();
        let dir = tempfile::tempdir().unwrap();
        let install_dir = dir.path().to_path_buf();
        let results = mgr.auto_install_new(&install_dir).await;
        assert!(results.is_empty());
    }

    #[test]
    fn reconciler_with_auto_install() {
        let reconciler = MarketplaceReconciler::with_auto_install(600);
        assert!(reconciler.auto_install);
        assert_eq!(reconciler.interval_secs, 600);
    }

    #[test]
    fn reconciliation_report_empty() {
        let report = ReconciliationReport {
            new_count: 0,
            updated_count: 0,
            removed_count: 0,
            new_plugins: vec![],
            updated_plugins: vec![],
            removed_plugins: vec![],
        };
        assert!(report.is_empty());
    }

    #[test]
    fn reconciliation_report_not_empty() {
        let report = ReconciliationReport {
            new_count: 1,
            updated_count: 0,
            removed_count: 0,
            new_plugins: vec![MarketplacePlugin {
                name: "new-plugin".into(),
                version: "1.0.0".into(),
                url: "https://example.com/new-plugin".into(),
                description: "A new plugin".into(),
            }],
            updated_plugins: vec![],
            removed_plugins: vec![],
        };
        assert!(!report.is_empty());
    }

    #[test]
    fn plugin_state_variants() {
        assert_eq!(PluginState::Installing, PluginState::Installing);
        assert_ne!(PluginState::Active, PluginState::Installing);
    }

    #[test]
    fn marketplace_rejects_http() {
        let mgr = PluginManager::new();
        let result = mgr.register_marketplace("bad".into(), "http://insecure.com".into());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("HTTPS"));
    }

    #[test]
    fn marketplace_rejects_malformed_url() {
        let mgr = PluginManager::new();
        let result = mgr.register_marketplace("bad".into(), "not-a-url".into());
        assert!(result.is_err());
    }

    #[test]
    fn marketplace_allowlist_blocks_unknown() {
        let mut mgr = PluginManager::new();
        mgr.set_marketplace_allowlist(vec!["trusted.com".into()]);
        let result = mgr.register_marketplace("untrusted".into(), "https://evil.com".into());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("allowlist"));
    }

    #[test]
    fn marketplace_allowlist_allows_trusted() {
        let mut mgr = PluginManager::new();
        mgr.set_marketplace_allowlist(vec!["trusted.com".into()]);
        let result = mgr.register_marketplace("trusted".into(), "https://trusted.com".into());
        assert!(result.is_ok());
    }

    #[test]
    fn version_compatibility_allows_compatible() {
        let mut mgr = PluginManager::new();
        mgr.set_app_version("1.0.0");
        assert!(mgr.is_version_compatible(Some("0.1.0")));
        assert!(mgr.is_version_compatible(Some("1.0.0")));
        assert!(mgr.is_version_compatible(None));
    }

    #[test]
    fn version_compatibility_rejects_incompatible() {
        let mut mgr = PluginManager::new();
        mgr.set_app_version("0.5.0");
        assert!(!mgr.is_version_compatible(Some("1.0.0")));
    }

    #[test]
    fn load_plugin_skips_incompatible_version() {
        let mgr = PluginManager::new();
        let manifest = PluginManifest {
            name: "old-plugin".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: Some("99.0.0".into()),
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        mgr.load_plugin(manifest, PathBuf::from("/tmp/old"));
        let plugin = mgr.get_plugin("old-plugin").unwrap();
        assert!(matches!(plugin.state, PluginState::Error(_)));
    }

    #[test]
    fn signature_verification_without_key_reports_that_nothing_was_checked() {
        let mgr = PluginManager::new();
        let result = mgr.verify_signature(b"any data", None);
        assert_eq!(result.unwrap(), SignatureCheck::Unverified);
    }

    #[test]
    fn signature_verification_requires_sig_when_key_set() {
        let mut mgr = PluginManager::new();
        // Dummy 32-byte hex key
        mgr.set_public_key("0000000000000000000000000000000000000000000000000000000000000000");
        let result = mgr.verify_signature(b"any data", None);
        assert!(result.is_err());
    }

    #[test]
    fn a_signature_that_does_not_match_the_key_is_refused() {
        let mut mgr = PluginManager::new();
        // The key ed25519-dalek signs with below, but the archive is different.
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        mgr.set_public_key(hex::encode(signing.verifying_key().to_bytes()));

        let signature = ed25519_dalek::Signer::sign(&signing, b"the real archive");
        let sig_hex = hex::encode(signature.to_bytes());

        assert_eq!(
            mgr.verify_signature(b"the real archive", Some(&sig_hex))
                .unwrap(),
            SignatureCheck::Verified
        );
        assert!(
            mgr.verify_signature(b"a swapped archive", Some(&sig_hex))
                .is_err()
        );
    }

    #[test]
    fn a_configured_signing_key_makes_signatures_mandatory() {
        let mut mgr = PluginManager::new();
        assert!(!mgr.apply_public_key(None));
        assert!(!mgr.apply_public_key(Some("   ".into())));
        // Nothing configured, so nothing is required of the plugin.
        assert_eq!(
            mgr.verify_signature(b"anything", None).unwrap(),
            SignatureCheck::Unverified
        );

        assert!(mgr.apply_public_key(Some(" abcd ".into())));
        assert_eq!(
            mgr.verify_signature(b"anything", None).unwrap_err(),
            "plugin signature missing but public key is configured"
        );
    }

    #[test]
    fn dependency_resolution_linear() {
        let mgr = PluginManager::new();
        let dir = tempfile::tempdir().unwrap();

        let base_dir = dir.path().join("base");
        let derived_dir = dir.path().join("derived");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::create_dir_all(&derived_dir).unwrap();

        let m1 = PluginManifest {
            name: "base".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        let m2 = PluginManifest {
            name: "derived".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: vec!["base".into()],
            signature: None,
            ..Default::default()
        };
        std::fs::write(
            base_dir.join("plugin.json"),
            serde_json::to_string(&m1).unwrap(),
        )
        .unwrap();
        std::fs::write(
            derived_dir.join("plugin.json"),
            serde_json::to_string(&m2).unwrap(),
        )
        .unwrap();

        mgr.load_plugin(m1, base_dir);
        mgr.load_plugin(m2, derived_dir);

        let order = mgr.resolve_load_order().unwrap();
        assert_eq!(order, vec!["base", "derived"]);
    }

    #[test]
    fn dependency_resolution_detects_cycle() {
        let mgr = PluginManager::new();
        let dir = tempfile::tempdir().unwrap();

        let a_dir = dir.path().join("a");
        let b_dir = dir.path().join("b");
        std::fs::create_dir_all(&a_dir).unwrap();
        std::fs::create_dir_all(&b_dir).unwrap();

        let m1 = PluginManifest {
            name: "a".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: vec!["b".into()],
            signature: None,
            ..Default::default()
        };
        let m2 = PluginManifest {
            name: "b".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: vec!["a".into()],
            signature: None,
            ..Default::default()
        };
        std::fs::write(
            a_dir.join("plugin.json"),
            serde_json::to_string(&m1).unwrap(),
        )
        .unwrap();
        std::fs::write(
            b_dir.join("plugin.json"),
            serde_json::to_string(&m2).unwrap(),
        )
        .unwrap();

        mgr.load_plugin(m1, a_dir);
        mgr.load_plugin(m2, b_dir);

        let result = mgr.resolve_load_order();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cycle"));
    }

    /// The retry backs off between attempts, as its documentation says it
    /// does. Without the backoff the loop hammered a broken plugin as fast as
    /// it was called.
    #[test]
    fn a_retry_waits_before_trying_again() {
        let mgr = PluginManager::new();
        let manifest = PluginManifest {
            name: "broken".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: Vec::new(),
            skills: Vec::new(),
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        mgr.load_plugin(manifest, std::path::PathBuf::from("/nonexistent/broken"));
        mgr.set_plugin_state("broken", PluginState::Error("broke".into()));

        // The first attempt happens immediately.
        mgr.retry_failed_plugins();
        assert_eq!(mgr.get_plugin("broken").unwrap().retry_count, 1);
        assert!(mgr.get_plugin("broken").unwrap().last_retry_at.is_some());

        // The next one does not, while the backoff is still running.
        mgr.retry_failed_plugins();
        assert_eq!(
            mgr.get_plugin("broken").unwrap().retry_count,
            1,
            "a retry happened without waiting"
        );
    }

    #[test]
    fn the_backoff_grows_and_is_capped() {
        assert_eq!(retry_backoff(0), std::time::Duration::from_secs(1));
        assert_eq!(retry_backoff(1), std::time::Duration::from_secs(2));
        assert_eq!(retry_backoff(2), std::time::Duration::from_secs(4));
        assert_eq!(retry_backoff(6), std::time::Duration::from_secs(60));
        assert_eq!(retry_backoff(20), std::time::Duration::from_secs(60));
    }

    #[test]
    fn retry_failed_plugins_recoverable() {
        let mgr = PluginManager::new();
        let manifest = PluginManifest {
            name: "broken".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: String::new().into(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            ..Default::default()
        };
        mgr.load_plugin(manifest.clone(), PathBuf::from("/tmp"));
        // Force into error state
        if let Some(mut entry) = mgr.plugins.get_mut("broken") {
            entry.state = PluginState::Error("test error".into());
            entry.path = PathBuf::from("/nonexistent");
        }

        mgr.retry_failed_plugins();
        let plugin = mgr.get_plugin("broken").unwrap();
        // Should still be error because manifest path doesn't exist,
        // but retry_count should have incremented
        assert_eq!(plugin.retry_count, 1);
    }

    /// The retry path is driven by a background loop, not by a caller
    /// remembering to invoke it. An Error-state plugin whose manifest is now
    /// readable must come back without anyone calling `retry_failed_plugins`
    /// directly.
    #[tokio::test]
    async fn the_retry_loop_recovers_an_error_state_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("recovering");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let manifest = PluginManifest {
            name: "recovering".into(),
            version: "1.0.0".into(),
            commands: vec!["hello".into()],
            skills: Vec::new(),
            author: String::new().into(),
            ..Default::default()
        };
        std::fs::write(
            plugin_dir.join("plugin.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let mut mgr = PluginManager::new();
        mgr.load_plugin(manifest, plugin_dir);
        mgr.set_plugin_state("recovering", PluginState::Error("manual".into()));
        // A recovered manifest must be followed by a real restart; the loop
        // cannot await one itself, so the host registers the callback.
        mgr.set_restart_handler(|_| Ok(()));
        let mgr = Arc::new(mgr);

        let loop_handle = mgr.spawn_retry_loop(std::time::Duration::from_millis(10));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    mgr.get_plugin("recovering").map(|plugin| plugin.state),
                    Some(PluginState::Active)
                ) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the retry loop never recovered the plugin");

        loop_handle.abort();
    }

    #[test]
    fn custom_endpoints_roundtrip() {
        let mgr = PluginManager::new();
        mgr.register_marketplace_with_endpoints(
            "custom".into(),
            "https://example.com".into(),
            "/v2/plugins".into(),
            "/v2/plugins/{name}/dl".into(),
        )
        .unwrap();

        let conn = mgr.marketplaces.get("custom").unwrap();
        assert_eq!(conn.list_endpoint, "/v2/plugins");
        assert_eq!(conn.download_endpoint, "/v2/plugins/{name}/dl");
    }

    // ── plugin.json field table ──────────────────────────────────────────

    /// A manifest shaped like Claude's parses, including the fields CodeRex
    /// does not act on yet.
    #[test]
    fn a_claude_shaped_manifest_parses() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{
                "$schema": "https://example.com/plugin.schema.json",
                "name": "shaped",
                "displayName": "Shaped Plugin",
                "version": "1.2.3",
                "description": "d",
                "author": {"name": "Someone", "email": "someone@example.com"},
                "homepage": "https://example.com",
                "repository": "https://github.com/example/shaped",
                "license": "MIT",
                "keywords": ["a", "b"],
                "defaultEnabled": true,
                "userConfig": {
                    "API_KEY": {"type": "string", "title": "API key", "sensitive": true},
                    "REGION": {"type": "string", "default": "eu"}
                },
                "experimental": {"themes": true},
                "mcpServers": {"browser": {"command": "node"}}
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.display_name.as_deref(), Some("Shaped Plugin"));
        assert_eq!(manifest.author.name, "Someone");
        assert_eq!(
            manifest.author.email.as_deref(),
            Some("someone@example.com")
        );
        assert_eq!(manifest.keywords, vec!["a", "b"]);
        assert_eq!(manifest.default_enabled, Some(true));
        assert!(manifest.user_config["API_KEY"].sensitive);
        assert_eq!(
            manifest.unimplemented_fields(),
            vec!["mcpServers", "experimental"],
            "fields with no CodeRex behaviour must be named, not dropped"
        );
    }

    /// The published Claude sample writes `author` as an object. A bare string
    /// is what CodeRex manifests use. Both have to parse: an object used to
    /// fail the whole manifest, which took the plugin with it, in silence.
    #[test]
    fn an_author_object_does_not_take_the_plugin_with_it() {
        let object: PluginManifest =
            serde_json::from_str(r#"{"name":"n","author":{"name":"A"}}"#).unwrap();
        assert_eq!(object.author.name, "A");

        let string: PluginManifest = serde_json::from_str(r#"{"name":"n","author":"B"}"#).unwrap();
        assert_eq!(string.author.to_string(), "B");
    }

    /// A misspelled field is reported by name instead of being dropped.
    #[test]
    fn an_unknown_manifest_field_is_an_error_not_a_silent_drop() {
        let err = serde_json::from_str::<PluginManifest>(
            r#"{"name":"n","displayname":"typo","userconfig":{}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("displayname"), "{err}");
    }

    // ── userConfig injection ─────────────────────────────────────────────

    fn manifest_with_options() -> PluginManifest {
        serde_json::from_str(
            r#"{
                "name": "configured",
                "entrypoint": "bin/plugin --region ${user_config.REGION}",
                "env": {"ENDPOINT": "https://${user_config.REGION}.example.com"},
                "userConfig": {
                    "TOKEN": {"type": "string", "required": true},
                    "REGION": {"type": "string", "default": "eu"}
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn user_config_becomes_plugin_option_variables() {
        let manifest = manifest_with_options();
        let values = manifest
            .resolve_options(&HashMap::new(), |key| {
                (key == "CODEREX_PLUGIN_OPTION_TOKEN").then(|| "from-env".to_string())
            })
            .unwrap();

        assert_eq!(values["TOKEN"], "from-env");
        assert_eq!(values["REGION"], "eu");

        let env = manifest.build_env(&values);
        assert_eq!(env["CODEREX_PLUGIN_OPTION_TOKEN"], "from-env");
        assert_eq!(env["CODEREX_PLUGIN_OPTION_REGION"], "eu");
        assert_eq!(env["ENDPOINT"], "https://eu.example.com");
    }

    #[test]
    fn user_config_substitution_fills_the_entrypoint() {
        let manifest = manifest_with_options();
        let values = manifest
            .resolve_options(&HashMap::new(), |key| {
                (key == "CODEREX_PLUGIN_OPTION_TOKEN").then(|| "value".to_string())
            })
            .unwrap();
        let entrypoint =
            substitute_user_config(manifest.entrypoint.as_deref().unwrap_or_default(), &values);
        assert_eq!(entrypoint, "bin/plugin --region eu");
    }

    #[test]
    fn a_value_supplied_by_the_host_beats_the_environment() {
        let manifest = manifest_with_options();
        let provided = HashMap::from([("TOKEN".to_string(), "from-host".to_string())]);
        let values = manifest
            .resolve_options(&provided, |_| Some("from-env".into()))
            .unwrap();
        assert_eq!(values["TOKEN"], "from-host");
    }

    #[test]
    fn a_sensitive_option_may_not_have_a_plaintext_default() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"name":"n","userConfig":{"API_KEY":{"sensitive":true,"default":"hunter2"}}}"#,
        )
        .unwrap();
        let err = manifest
            .resolve_options(&HashMap::new(), |_| None)
            .unwrap_err();
        assert!(err.contains("plaintext default"), "{err}");
    }

    #[test]
    fn an_unresolved_keyring_reference_is_refused() {
        let manifest: PluginManifest =
            serde_json::from_str(r#"{"name":"n","userConfig":{"API_KEY":{"sensitive":true}}}"#)
                .unwrap();
        let provided =
            HashMap::from([("API_KEY".to_string(), "keyring://coderex/api".to_string())]);
        let err = manifest.resolve_options(&provided, |_| None).unwrap_err();
        assert!(err.contains("keyring://coderex/api"), "{err}");
    }

    #[test]
    fn a_required_option_with_no_value_stops_the_plugin() {
        let manifest = manifest_with_options();
        let err = manifest
            .resolve_options(&HashMap::new(), |_| None)
            .unwrap_err();
        assert!(err.contains("required"), "{err}");
        assert!(err.contains("CODEREX_PLUGIN_OPTION_TOKEN"), "{err}");
    }

    #[test]
    fn an_optional_option_with_no_value_is_simply_absent() {
        let manifest: PluginManifest =
            serde_json::from_str(r#"{"name":"n","userConfig":{"REGION":{"type":"string"}}}"#)
                .unwrap();
        let values = manifest.resolve_options(&HashMap::new(), |_| None).unwrap();
        assert!(values.is_empty());
        assert!(manifest.build_env(&values).is_empty());
    }

    #[test]
    fn an_option_name_that_cannot_be_an_environment_variable_is_refused() {
        let manifest: PluginManifest =
            serde_json::from_str(r#"{"name":"n","userConfig":{"bad-key":{}}}"#).unwrap();
        let err = manifest
            .resolve_options(&HashMap::new(), |_| None)
            .unwrap_err();
        assert!(err.contains("bad-key"), "{err}");
    }

    /// WASM plugins resolve `userConfig` through the same code path as process
    /// plugins. A missing required option therefore fails the spawn *before*
    /// the module is loaded, even though a WASM module has no environment to
    /// fall back to.
    #[tokio::test]
    async fn wasm_plugin_required_user_config_is_enforced_before_load() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_json = r#"{
            "name": "wasm-config",
            "runtime": "wasm",
            "entrypoint": "plugin.wasm",
            "userConfig": {"TOKEN": {"type": "string", "required": true}}
        }"#;
        std::fs::write(dir.path().join("plugin.json"), manifest_json).unwrap();
        let manifest: PluginManifest = serde_json::from_str(manifest_json).unwrap();

        let mgr = PluginManager::new();
        mgr.load_plugin(manifest, dir.path().to_path_buf());

        let err = mgr
            .spawn_plugin("wasm-config")
            .await
            .expect_err("a required option with no value must stop a WASM spawn");
        assert!(err.contains("userConfig"), "{err}");
        assert!(err.contains("TOKEN"), "{err}");
    }

    /// The environment really reaches the child process: a plugin script that
    /// dumps its environment and answers JSON-RPC.
    #[tokio::test]
    async fn user_config_reaches_the_plugin_process() {
        let dir = tempfile::tempdir().unwrap();
        let dumped = dir.path().join("env.txt");
        let script = dir.path().join("plugin.sh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
env > {dumped}
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{{"jsonrpc":"2.0","id":%s,"result":[]}}\n' "$id"
done
"#,
                dumped = dumped.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest_json = format!(
            r#"{{"name":"env-plugin","entrypoint":"{}","userConfig":{{"TOKEN":{{"required":true}}}}}}"#,
            script.display()
        );
        std::fs::write(dir.path().join("plugin.json"), &manifest_json).unwrap();
        let manifest: PluginManifest = serde_json::from_str(&manifest_json).unwrap();

        let mgr = PluginManager::new();
        mgr.set_plugin_option("env-plugin", "TOKEN", "s3cret");
        mgr.load_plugin(manifest, dir.path().to_path_buf());

        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            mgr.spawn_plugin("env-plugin"),
        )
        .await
        .expect("spawning the plugin timed out")
        .expect("plugin should spawn");

        let environment = std::fs::read_to_string(&dumped).unwrap();
        assert!(
            environment.contains("CODEREX_PLUGIN_OPTION_TOKEN=s3cret"),
            "the option never reached the child: {environment}"
        );
    }

    // ── marketplace sources ──────────────────────────────────────────────

    #[test]
    fn source_detection_picks_the_right_kind() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            PluginSource::detect(&dir.path().to_string_lossy()),
            PluginSource::LocalDir(_)
        ));
        for git in [
            "https://github.com/acme/plugin.git",
            "https://github.com/acme/plugin",
            "git@github.com:acme/plugin.git",
            "ssh://git@example.com/acme/plugin.git",
        ] {
            assert!(
                matches!(PluginSource::detect(git), PluginSource::Git { .. }),
                "{git} should be a git source"
            );
        }
        assert!(matches!(
            PluginSource::detect("https://example.com/plugin.tar.gz"),
            PluginSource::Archive { .. }
        ));
        assert_eq!(
            PluginSource::detect("some-plugin"),
            PluginSource::Marketplace {
                name: "some-plugin".into()
            }
        );
    }

    #[test]
    fn a_git_transport_that_runs_commands_is_refused() {
        let err = validate_git_url("ext::sh -c 'touch /tmp/pwned'").unwrap_err();
        // The transport is what has to be named: falling through to the scheme
        // allowlist would also reject it, but for the wrong reason.
        assert!(err.contains("ext:: transport"), "{err}");
        assert!(validate_git_url("https://github.com/acme/plugin.git").is_ok());
        assert!(validate_git_url("git@github.com:acme/plugin.git").is_ok());
        assert!(validate_git_url("http://example.com/plugin.git").is_err());
    }

    #[tokio::test]
    async fn a_non_https_archive_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_archive("http://example.com/plugin.tar.gz", None, dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("non-HTTPS"), "{err}");
    }

    #[tokio::test]
    async fn a_marketplace_listing_is_not_installed_as_a_source() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::Marketplace {
                    name: "thing".into(),
                },
                dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("marketplace listing"), "{err}");
    }

    // ── installing from a directory ──────────────────────────────────────

    fn write_local_plugin(dir: &Path, name: &str) {
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn installing_from_a_local_directory_copies_it_into_place() {
        let source = tempfile::tempdir().unwrap();
        write_local_plugin(source.path(), "local-plugin");
        let install_dir = tempfile::tempdir().unwrap();

        let mgr = PluginManager::new();
        let installed = mgr
            .install_from_source(
                &PluginSource::LocalDir(source.path().to_path_buf()),
                install_dir.path(),
            )
            .await
            .unwrap();

        assert_eq!(installed, install_dir.path().join("local-plugin"));
        assert!(installed.join("plugin.json").exists());
        // The source directory is left alone: it is someone's working copy.
        assert!(source.path().join("plugin.json").exists());
        assert!(mgr.needs_refresh());
        let handle = mgr.get_plugin("local-plugin").unwrap();
        assert_eq!(handle.state, PluginState::Active);
        // A directory has no archive to sign, so nothing was verified.
        assert!(!handle.verified);
        // Nothing is left staged behind.
        assert!(!install_dir.path().join(".install-local-plugin").exists());
    }

    #[tokio::test]
    async fn a_second_install_of_the_same_plugin_is_refused() {
        let source = tempfile::tempdir().unwrap();
        write_local_plugin(source.path(), "local-plugin");
        let install_dir = tempfile::tempdir().unwrap();

        let mgr = PluginManager::new();
        let source = PluginSource::LocalDir(source.path().to_path_buf());
        mgr.install_from_source(&source, install_dir.path())
            .await
            .unwrap();
        let err = mgr
            .install_from_source(&source, install_dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("already installed"), "{err}");
    }

    /// What `coderex plugin install <source>` does: the argument is passed
    /// through as both the name and the url of a listing, and a listing whose
    /// url names a directory installs from that directory.
    #[tokio::test]
    async fn the_cli_install_path_reaches_the_new_sources() {
        let source = tempfile::tempdir().unwrap();
        write_local_plugin(source.path(), "from-cli");
        let install_dir = tempfile::tempdir().unwrap();
        let argument = source.path().to_string_lossy().to_string();

        let mgr = PluginManager::new();
        let listing = MarketplacePlugin {
            name: argument.clone(),
            version: "latest".into(),
            url: argument,
            description: String::new(),
        };

        let installed = mgr
            .install_plugin(&listing, &install_dir.path().to_path_buf())
            .await
            .unwrap();

        assert_eq!(installed, install_dir.path().join("from-cli"));
        assert!(installed.join("plugin.json").exists());
    }

    #[tokio::test]
    async fn installing_a_directory_without_a_manifest_fails() {
        let source = tempfile::tempdir().unwrap();
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::LocalDir(source.path().to_path_buf()),
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("no plugin.json"), "{err}");
    }

    /// The name comes from the plugin's own manifest, so it must not be able to
    /// point the install at a directory outside the install root.
    #[tokio::test]
    async fn a_plugin_name_that_escapes_the_install_directory_is_refused() {
        let source = tempfile::tempdir().unwrap();
        write_local_plugin(source.path(), "../../escaped");
        let install_dir = tempfile::tempdir().unwrap();
        let inside = install_dir.path().join("plugins");
        std::fs::create_dir_all(&inside).unwrap();

        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::LocalDir(source.path().to_path_buf()),
                &inside,
            )
            .await
            .unwrap_err();

        assert!(err.contains("directory name"), "{err}");
        assert!(!install_dir.path().join("escaped").exists());
    }

    // ── installing from git ──────────────────────────────────────────────

    /// Create a throwaway repository in a temporary directory and return its
    /// path and HEAD commit.
    ///
    /// The repository is made inside `tempfile::tempdir()`, and every command
    /// is pinned with `-C`, so nothing here touches the project's own repo.
    async fn run_in_repo(path: &str, args: &[&str]) -> Result<String, String> {
        let mut full: Vec<&str> = vec![
            "-C",
            path,
            "-c",
            "user.name=coderex",
            "-c",
            "user.email=coderex@example.invalid",
        ];
        full.extend_from_slice(args);
        run_git(&full).await
    }

    async fn fixture_repo(name: &str) -> Option<(tempfile::TempDir, String)> {
        if run_git(&["--version"]).await.is_err() {
            return None;
        }
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().to_string_lossy().to_string();

        run_in_repo(&path, &["init", "-q"]).await.ok()?;
        write_local_plugin(repo.path(), name);
        run_in_repo(&path, &["add", "-A"]).await.ok()?;
        run_in_repo(&path, &["commit", "-q", "-m", "plugin"])
            .await
            .ok()?;
        let commit = run_in_repo(&path, &["rev-parse", "HEAD"])
            .await
            .ok()?
            .trim()
            .to_string();
        Some((repo, commit))
    }

    #[tokio::test]
    async fn installing_from_git_clones_the_repo_and_pins_the_commit() {
        let Some((repo, commit)) = fixture_repo("git-plugin").await else {
            eprintln!("skipping: git is not available");
            return;
        };
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();

        let installed = mgr
            .install_from_source(
                &PluginSource::Git {
                    url: format!("file://{}", repo.path().display()),
                    rev: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap();

        assert_eq!(installed, install_dir.path().join("git-plugin"));
        assert!(installed.join("plugin.json").exists());
        // The checkout is a snapshot, not a nested repository.
        assert!(!installed.join(".git").exists());

        let pins = PluginPinStore::load(&install_dir.path().join(PIN_FILE_NAME)).unwrap();
        assert_eq!(
            pins.get("git-plugin").unwrap().git_commit.as_deref(),
            Some(commit.as_str())
        );
    }

    #[tokio::test]
    async fn a_git_install_that_cannot_clone_reports_why() {
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::Git {
                    url: "https://example.invalid/does/not/exist.git".into(),
                    rev: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("git"), "{err}");
        // Nothing is left behind for the next attempt to trip over.
        assert!(
            !install_dir
                .path()
                .join(".install-https___example.invalid_does_not_exist_git")
                .exists()
        );
    }

    // ── github / git-subdir / unsupported sources ────────────────────────

    #[test]
    fn structured_source_specs_are_detected() {
        assert_eq!(
            PluginSource::detect("github:acme/plugin"),
            PluginSource::Github {
                repo: "acme/plugin".into(),
                rev: None,
                sha: None,
            }
        );
        // A branch or tag is a `rev`; a full commit is a `sha`.
        assert_eq!(
            PluginSource::detect("github:acme/plugin@main"),
            PluginSource::Github {
                repo: "acme/plugin".into(),
                rev: Some("main".into()),
                sha: None,
            }
        );
        let commit = "a".repeat(40);
        assert_eq!(
            PluginSource::detect(&format!("github:acme/plugin@{commit}")),
            PluginSource::Github {
                repo: "acme/plugin".into(),
                rev: None,
                sha: Some(commit),
            }
        );

        assert_eq!(
            PluginSource::detect("git-subdir:https://github.com/acme/mono.git#plugins/foo@v1"),
            PluginSource::GitSubdir {
                url: "https://github.com/acme/mono.git".into(),
                path: "plugins/foo".into(),
                rev: Some("v1".into()),
                sha: None,
            }
        );
        // The `@` belongs to the scp-like clone URL, not to a revision.
        assert_eq!(
            PluginSource::detect("git-subdir:git@github.com:acme/mono.git#plugins/foo"),
            PluginSource::GitSubdir {
                url: "git@github.com:acme/mono.git".into(),
                path: "plugins/foo".into(),
                rev: None,
                sha: None,
            }
        );

        // The two kinds the reference defines but CodeRex does not implement are
        // recognised, so they are never mistaken for a plugin name.
        assert_eq!(
            PluginSource::detect("npm:@acme/plugin"),
            PluginSource::Unsupported { kind: "npm".into() }
        );
        assert_eq!(
            PluginSource::detect("command:print-the-path"),
            PluginSource::Unsupported {
                kind: "command".into()
            }
        );
        // A bare word is still a marketplace listing, not a command.
        assert!(matches!(
            PluginSource::detect("command"),
            PluginSource::Marketplace { .. }
        ));
    }

    #[test]
    fn github_shorthand_becomes_a_clone_url_and_the_rest_is_refused() {
        assert_eq!(
            github_clone_url("acme/plugin").unwrap(),
            "https://github.com/acme/plugin.git"
        );
        assert_eq!(
            github_clone_url("acme/my.plugin-2_x").unwrap(),
            "https://github.com/acme/my.plugin-2_x.git"
        );
        for bad in [
            "plugin",
            "acme/plugin/extra",
            "/plugin",
            "acme/",
            "../evil",
            "acme/../evil",
            "acme/pl ug in",
        ] {
            assert!(github_clone_url(bad).is_err(), "{bad} should be refused");
        }
    }

    #[tokio::test]
    async fn a_github_source_that_is_not_owner_repo_is_refused() {
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::Github {
                    repo: "just-a-name".into(),
                    rev: None,
                    sha: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("owner/repo form"), "{err}");
    }

    /// A caller that asked for a commit must get that commit or an error, never
    /// a branch that happens to share the name.
    #[tokio::test]
    async fn a_source_sha_that_is_not_a_commit_is_refused() {
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::GitSubdir {
                    url: "file:///nonexistent/plugin-repo".into(),
                    path: "plugins/foo".into(),
                    rev: None,
                    sha: Some("v1".into()),
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("40-character commit SHA"), "{err}");

        let err = mgr
            .install_from_source(
                &PluginSource::Github {
                    repo: "acme/plugin".into(),
                    rev: Some("main".into()),
                    sha: Some("main".into()),
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("40-character commit SHA"), "{err}");
    }

    /// A repository whose plugin lives in a subdirectory, so the root itself is
    /// not a valid plugin.
    async fn fixture_repo_with_subdir(
        subdir: &str,
        name: &str,
    ) -> Option<(tempfile::TempDir, String)> {
        if run_git(&["--version"]).await.is_err() {
            return None;
        }
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().to_string_lossy().to_string();

        run_in_repo(&path, &["init", "-q"]).await.ok()?;
        std::fs::write(repo.path().join("README.md"), "not a plugin").unwrap();
        let plugin_dir = repo.path().join(subdir);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        write_local_plugin(&plugin_dir, name);
        run_in_repo(&path, &["add", "-A"]).await.ok()?;
        run_in_repo(&path, &["commit", "-q", "-m", "plugin"])
            .await
            .ok()?;
        let commit = run_in_repo(&path, &["rev-parse", "HEAD"])
            .await
            .ok()?
            .trim()
            .to_string();
        Some((repo, commit))
    }

    #[tokio::test]
    async fn installing_from_a_git_subdirectory_installs_that_subdirectory() {
        let Some((repo, commit)) = fixture_repo_with_subdir("plugins/inner", "sub-plugin").await
        else {
            eprintln!("skipping: git is not available");
            return;
        };
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();

        let installed = mgr
            .install_from_source(
                &PluginSource::GitSubdir {
                    url: format!("file://{}", repo.path().display()),
                    path: "plugins/inner".into(),
                    rev: None,
                    sha: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap();

        assert_eq!(installed, install_dir.path().join("sub-plugin"));
        assert!(installed.join("plugin.json").exists());
        // Exactly the subdirectory was installed: the repository's own files did
        // not come along, and the checkout is still a snapshot.
        assert!(!installed.join("README.md").exists());
        assert!(!installed.join(".git").exists());
        // The clone and the staging directory are both gone.
        assert!(!install_dir.path().join(".install-sub-plugin").exists());
        let leftovers: Vec<_> = std::fs::read_dir(install_dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".install-"))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");

        // The pin is the commit the clone resolved to, exactly as a git install.
        let pins = PluginPinStore::load(&install_dir.path().join(PIN_FILE_NAME)).unwrap();
        assert_eq!(
            pins.get("sub-plugin").unwrap().git_commit.as_deref(),
            Some(commit.as_str())
        );
    }

    #[tokio::test]
    async fn a_git_subdir_path_that_escapes_the_clone_is_refused() {
        let Some((repo, _)) = fixture_repo_with_subdir("plugins/inner", "sub-plugin").await else {
            eprintln!("skipping: git is not available");
            return;
        };
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let url = format!("file://{}", repo.path().display());

        for path in ["..", "../..", "/etc", "plugins/../../etc"] {
            let err = mgr
                .install_from_source(
                    &PluginSource::GitSubdir {
                        url: url.clone(),
                        path: path.into(),
                        rev: None,
                        sha: None,
                    },
                    install_dir.path(),
                )
                .await
                .unwrap_err();
            assert!(
                err.contains("climbs out") || err.contains("absolute"),
                "{path}: {err}"
            );
        }

        // Nothing was installed, and no staging directory was left behind for
        // the next attempt to trip over.
        assert!(mgr.list_plugins().is_empty());
        let leftovers: Vec<_> = std::fs::read_dir(install_dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_git_subdir_symlink_that_leaves_the_clone_is_refused() {
        let Some((repo, _)) = fixture_repo_with_subdir("plugins/inner", "sub-plugin").await else {
            eprintln!("skipping: git is not available");
            return;
        };
        // A committed symlink pointing outside the checkout: the path itself is
        // innocent, so only the canonicalised check catches it.
        let escape_target = repo.path().parent().unwrap().to_path_buf();
        std::os::unix::fs::symlink(&escape_target, repo.path().join("escape")).unwrap();
        let path = repo.path().to_string_lossy().to_string();
        run_in_repo(&path, &["add", "-A"]).await.unwrap();
        run_in_repo(&path, &["commit", "-q", "-m", "symlink"])
            .await
            .unwrap();

        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::GitSubdir {
                    url: format!("file://{}", repo.path().display()),
                    path: "escape".into(),
                    rev: None,
                    sha: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("does not resolve inside"), "{err}");
    }

    #[tokio::test]
    async fn a_git_subdir_path_that_does_not_exist_is_refused() {
        let Some((repo, _)) = fixture_repo_with_subdir("plugins/inner", "sub-plugin").await else {
            eprintln!("skipping: git is not available");
            return;
        };
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::GitSubdir {
                    url: format!("file://{}", repo.path().display()),
                    path: "plugins/missing".into(),
                    rev: None,
                    sha: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("not a directory in the repository"), "{err}");

        // An empty path is a spec that cannot be resolved, not a root install.
        let err = mgr
            .install_from_source(
                &PluginSource::GitSubdir {
                    url: format!("file://{}", repo.path().display()),
                    path: String::new(),
                    rev: None,
                    sha: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("empty path"), "{err}");
    }

    /// The `command` source runs a command named by a marketplace entry. CodeRex
    /// has no sandbox for that, so the kind is refused and nothing runs.
    #[tokio::test]
    async fn a_command_source_is_refused_and_never_executed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();

        let source = PluginSource::detect(&format!("command:touch {}", marker.display()));
        assert!(
            matches!(source, PluginSource::Unsupported { .. }),
            "a command spec must not become an installable source"
        );
        let err = mgr
            .install_from_source(&source, install_dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("sandbox"), "{err}");
        assert!(!marker.exists(), "the command was executed");
    }

    #[tokio::test]
    async fn the_npm_source_reports_the_reference_gap() {
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        let err = mgr
            .install_from_source(
                &PluginSource::detect("npm:@acme/plugin"),
                install_dir.path(),
            )
            .await
            .unwrap_err();
        // The reference's own message, so the gap is attributed, not invented.
        assert!(
            err.contains("NPM marketplace sources not yet implemented"),
            "{err}"
        );
    }

    #[test]
    fn a_branch_rev_is_overridden_by_an_explicit_commit() {
        let commit = "c".repeat(40);
        assert_eq!(
            checkout_rev(Some("main"), Some(&commit)).unwrap(),
            Some(commit.as_str())
        );
        assert_eq!(checkout_rev(Some("main"), None).unwrap(), Some("main"));
        assert_eq!(checkout_rev(None, None).unwrap(), None);
    }

    // ── pinning ──────────────────────────────────────────────────────────

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn a_pin_records_the_first_install_and_refuses_a_different_one() {
        let install_dir = tempfile::tempdir().unwrap();
        let first = PluginPin {
            sha256: Some("aa".repeat(32)),
            git_commit: None,
        };

        // Trust on first use: nothing is recorded, so nothing is refused.
        let store = PluginManager::check_pin(install_dir.path(), "pinned", &first).unwrap();
        assert!(store.get("pinned").is_none());
        PluginManager::record_pin(install_dir.path(), store, "pinned", &first);

        let on_disk = PluginPinStore::load(&install_dir.path().join(PIN_FILE_NAME)).unwrap();
        assert_eq!(on_disk.get("pinned"), Some(&first));

        // The same contents install again.
        assert!(PluginManager::check_pin(install_dir.path(), "pinned", &first).is_ok());

        // Different contents do not.
        let swapped = PluginPin {
            sha256: Some("bb".repeat(32)),
            git_commit: None,
        };
        let err = PluginManager::check_pin(install_dir.path(), "pinned", &swapped).unwrap_err();
        assert!(err.contains("pinned SHA-256"), "{err}");
    }

    #[test]
    fn a_pin_file_that_cannot_be_read_is_an_error() {
        let install_dir = tempfile::tempdir().unwrap();
        std::fs::write(install_dir.path().join(PIN_FILE_NAME), "not json").unwrap();
        let pin = PluginPin {
            sha256: Some("aa".repeat(32)),
            git_commit: None,
        };
        // Forgetting the pins would turn the next install into a fresh first
        // use, which is exactly what the pin is for.
        assert!(PluginManager::check_pin(install_dir.path(), "pinned", &pin).is_err());
    }

    #[test]
    fn a_commit_pin_is_not_checked_against_an_archive_hash() {
        let commit_pin = PluginPin {
            sha256: None,
            git_commit: Some("a".repeat(40)),
        };
        let archive = PluginPin {
            sha256: Some("b".repeat(64)),
            git_commit: None,
        };
        // The pinned fields are the ones that are checked, so a plugin pinned
        // by commit is not failed for lacking a hash it never had.
        assert!(commit_pin.check(&archive).is_err());
        assert!(archive.check(&archive).is_ok());
        assert!(PluginPin::default().check(&archive).is_ok());
    }

    // ── detached archive signatures ──────────────────────────────────────

    /// Build a one-file `.tar.gz` plugin archive in memory.
    fn plugin_archive(manifest_json: &str) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest_json.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "plugin.json", manifest_json.as_bytes())
                .unwrap();
            builder.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    fn test_signing_key() -> ed25519_dalek::SigningKey {
        // A deterministic test key: no RNG feature is required, and the same
        // bytes make the child-process test below reproducible.
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
    }

    fn sign_archive(key: &ed25519_dalek::SigningKey, archive: &[u8]) -> String {
        hex::encode(ed25519_dalek::Signer::sign(key, archive).to_bytes())
    }

    #[tokio::test]
    async fn a_detached_signature_over_the_archive_installs_it() {
        let archive = plugin_archive(r#"{"name":"signed-plugin","version":"1.0.0"}"#);
        let key = test_signing_key();
        let signature = sign_archive(&key, &archive);

        let mut mgr = PluginManager::new();
        mgr.set_public_key(hex::encode(key.verifying_key().to_bytes()));
        let install_dir = tempfile::tempdir().unwrap();

        let installed = mgr
            .install_archive_bytes(&archive, Some(&signature), install_dir.path())
            .await
            .unwrap();

        assert_eq!(installed, install_dir.path().join("signed-plugin"));
        let handle = mgr.get_plugin("signed-plugin").unwrap();
        assert_eq!(handle.state, PluginState::Active);
        assert!(
            handle.verified,
            "a matching detached signature was not recorded"
        );
    }

    #[tokio::test]
    async fn a_detached_signature_for_different_bytes_is_refused() {
        let archive = plugin_archive(r#"{"name":"signed-plugin","version":"1.0.0"}"#);
        let key = test_signing_key();
        let signature = sign_archive(&key, b"a different archive");

        let mut mgr = PluginManager::new();
        mgr.set_public_key(hex::encode(key.verifying_key().to_bytes()));
        let install_dir = tempfile::tempdir().unwrap();

        let err = mgr
            .install_archive_bytes(&archive, Some(&signature), install_dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("signature verification failed"), "{err}");
        assert!(!install_dir.path().join("signed-plugin").exists());
        assert!(mgr.list_plugins().is_empty());
    }

    #[tokio::test]
    async fn a_missing_detached_signature_is_refused_when_a_key_is_configured() {
        let archive = plugin_archive(r#"{"name":"signed-plugin","version":"1.0.0"}"#);
        let key = test_signing_key();

        let mut mgr = PluginManager::new();
        mgr.set_public_key(hex::encode(key.verifying_key().to_bytes()));
        let install_dir = tempfile::tempdir().unwrap();

        let err = mgr
            .install_archive_bytes(&archive, None, install_dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("plugin signature missing"), "{err}");
        assert!(!install_dir.path().join("signed-plugin").exists());
    }

    #[tokio::test]
    async fn without_a_key_an_archive_still_installs_unverified() {
        let archive = plugin_archive(r#"{"name":"unsigned-plugin","version":"1.0.0"}"#);
        let mgr = PluginManager::new();
        let install_dir = tempfile::tempdir().unwrap();

        let installed = mgr
            .install_archive_bytes(&archive, None, install_dir.path())
            .await
            .unwrap();
        assert!(installed.join("plugin.json").exists());
        assert!(!mgr.get_plugin("unsigned-plugin").unwrap().verified);
    }

    // ── unverifiable sources with a configured key ───────────────────────

    fn configure_test_key(mgr: &mut PluginManager) -> String {
        let key = test_signing_key();
        mgr.set_public_key(hex::encode(key.verifying_key().to_bytes()));
        hex::encode(key.verifying_key().to_bytes())
    }

    #[tokio::test]
    async fn a_configured_key_refuses_an_unpinned_directory_install() {
        let source = tempfile::tempdir().unwrap();
        write_local_plugin(source.path(), "local-plugin");
        let install_dir = tempfile::tempdir().unwrap();

        let mut mgr = PluginManager::new();
        configure_test_key(&mut mgr);

        let err = mgr
            .install_from_source(
                &PluginSource::LocalDir(source.path().to_path_buf()),
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("pinned by trust-on-first-use"), "{err}");
        assert!(!install_dir.path().join("local-plugin").exists());
        assert!(mgr.list_plugins().is_empty());
    }

    #[tokio::test]
    async fn a_directory_pinned_by_tofu_installs_with_a_configured_key() {
        let source = tempfile::tempdir().unwrap();
        write_local_plugin(source.path(), "local-plugin");
        let install_dir = tempfile::tempdir().unwrap();

        // A previous (unkeyed) install would have recorded this pin.
        let digest = PluginManager::directory_digest(source.path()).unwrap();
        let store = PluginPinStore {
            plugins: HashMap::from([(
                "local-plugin".to_string(),
                PluginPin {
                    sha256: Some(digest),
                    git_commit: None,
                },
            )]),
        };
        store.save(&install_dir.path().join(PIN_FILE_NAME)).unwrap();

        let mut mgr = PluginManager::new();
        configure_test_key(&mut mgr);
        let installed = mgr
            .install_from_source(
                &PluginSource::LocalDir(source.path().to_path_buf()),
                install_dir.path(),
            )
            .await
            .unwrap();
        assert!(installed.join("plugin.json").exists());
        // Pinned, not signature-checked.
        assert!(!mgr.get_plugin("local-plugin").unwrap().verified);
    }

    #[tokio::test]
    async fn a_configured_key_refuses_an_unpinned_git_install() {
        let Some((repo, _)) = fixture_repo("git-plugin").await else {
            eprintln!("skipping: git is not available");
            return;
        };
        let install_dir = tempfile::tempdir().unwrap();
        let mut mgr = PluginManager::new();
        configure_test_key(&mut mgr);

        let err = mgr
            .install_from_source(
                &PluginSource::Git {
                    url: format!("file://{}", repo.path().display()),
                    rev: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("pinned by trust-on-first-use"), "{err}");
        assert!(!install_dir.path().join("git-plugin").exists());
    }

    #[tokio::test]
    async fn a_git_install_pinned_by_tofu_is_allowed_with_a_configured_key() {
        let Some((repo, commit)) = fixture_repo("git-plugin").await else {
            eprintln!("skipping: git is not available");
            return;
        };
        let install_dir = tempfile::tempdir().unwrap();
        let store = PluginPinStore {
            plugins: HashMap::from([(
                "git-plugin".to_string(),
                PluginPin {
                    sha256: None,
                    git_commit: Some(commit),
                },
            )]),
        };
        store.save(&install_dir.path().join(PIN_FILE_NAME)).unwrap();

        let mut mgr = PluginManager::new();
        configure_test_key(&mut mgr);
        let installed = mgr
            .install_from_source(
                &PluginSource::Git {
                    url: format!("file://{}", repo.path().display()),
                    rev: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap();
        assert!(installed.join("plugin.json").exists());
    }

    #[tokio::test]
    async fn a_configured_key_refuses_an_unpinned_git_subdir_install() {
        let Some((repo, _)) = fixture_repo_with_subdir("plugins/inner", "sub-plugin").await else {
            eprintln!("skipping: git is not available");
            return;
        };
        let install_dir = tempfile::tempdir().unwrap();
        let mut mgr = PluginManager::new();
        configure_test_key(&mut mgr);

        let err = mgr
            .install_from_source(
                &PluginSource::GitSubdir {
                    url: format!("file://{}", repo.path().display()),
                    path: "plugins/inner".into(),
                    rev: None,
                    sha: None,
                },
                install_dir.path(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("pinned by trust-on-first-use"), "{err}");
        assert!(!install_dir.path().join("sub-plugin").exists());
    }

    // ── retry is backed by a real spawn ──────────────────────────────────

    #[tokio::test]
    async fn a_spawn_failure_is_recorded_as_an_error_state() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_json = r#"{"name":"unspawnable","entrypoint":"coderex-no-such-binary-9d8f"}"#;
        std::fs::write(dir.path().join("plugin.json"), manifest_json).unwrap();
        let manifest: PluginManifest = serde_json::from_str(manifest_json).unwrap();

        let mgr = PluginManager::new();
        mgr.load_plugin(manifest, dir.path().to_path_buf());

        assert!(mgr.spawn_plugin("unspawnable").await.is_err());
        let state = mgr.get_plugin("unspawnable").unwrap().state;
        assert!(
            matches!(state, PluginState::Error(_)),
            "a failed spawn left the plugin in {state:?}"
        );
    }

    fn write_recoverable_plugin(dir: &Path, name: &str) -> PluginManifest {
        let manifest_json =
            format!(r#"{{"name":"{name}","version":"1.0.0","commands":["hello"]}}"#);
        std::fs::write(dir.join("plugin.json"), &manifest_json).unwrap();
        serde_json::from_str(&manifest_json).unwrap()
    }

    #[test]
    fn a_successful_retry_invokes_the_restart_callback() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_recoverable_plugin(dir.path(), "restartable");

        let mut mgr = PluginManager::new();
        mgr.load_plugin(manifest, dir.path().to_path_buf());
        mgr.set_plugin_state("restartable", PluginState::Error("broke".into()));

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let calls = Arc::clone(&calls);
            let seen = Arc::clone(&seen);
            mgr.set_restart_handler(move |name| {
                calls.fetch_add(1, Ordering::SeqCst);
                seen.lock().unwrap().push(name.to_string());
                Ok(())
            });
        }

        let outcomes = mgr.retry_failed_plugins();
        assert_eq!(
            outcomes,
            vec![RetryOutcome::Restarted("restartable".into())]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*seen.lock().unwrap(), vec!["restartable".to_string()]);
        assert_eq!(
            mgr.get_plugin("restartable").unwrap().state,
            PluginState::Active
        );
    }

    #[test]
    fn a_retry_without_a_restart_handler_reports_that_a_restart_is_needed() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_recoverable_plugin(dir.path(), "needs-spawn");

        let mgr = PluginManager::new();
        mgr.load_plugin(manifest, dir.path().to_path_buf());
        mgr.set_plugin_state("needs-spawn", PluginState::Error("broke".into()));

        let outcomes = mgr.retry_failed_plugins();
        assert_eq!(
            outcomes,
            vec![RetryOutcome::RestartRequired("needs-spawn".into())]
        );
        // Not Active: nothing has spawned it yet.
        assert_eq!(
            mgr.get_plugin("needs-spawn").unwrap().state,
            PluginState::Loading
        );
    }

    #[test]
    fn a_retry_whose_restart_fails_stays_in_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_recoverable_plugin(dir.path(), "still-broken");

        let mut mgr = PluginManager::new();
        mgr.load_plugin(manifest, dir.path().to_path_buf());
        mgr.set_plugin_state("still-broken", PluginState::Error("broke".into()));
        mgr.set_restart_handler(|_| Err("spawn exploded".into()));

        let outcomes = mgr.retry_failed_plugins();
        assert_eq!(
            outcomes,
            vec![RetryOutcome::StillFailed("still-broken".into())]
        );
        let state = mgr.get_plugin("still-broken").unwrap().state;
        assert!(
            matches!(state, PluginState::Error(ref message) if message.contains("restart failed")),
            "{state:?}"
        );
    }

    // ── host-supplied userConfig from settings ───────────────────────────

    #[test]
    fn host_options_from_a_map_are_applied() {
        let mgr = PluginManager::new();
        let options = HashMap::from([(
            "configured".to_string(),
            HashMap::from([("TOKEN".to_string(), "from-settings".to_string())]),
        )]);
        mgr.apply_options_from_map(&options);

        assert_eq!(mgr.plugin_options("configured")["TOKEN"], "from-settings");
    }

    #[test]
    fn options_from_settings_json_are_applied_and_stringified() {
        let mgr = PluginManager::new();
        let settings = serde_json::json!({
            "plugins": {
                "configured": {
                    "options": {
                        "TOKEN": "from-settings",
                        "PORT": 8080,
                        "DEBUG": true
                    }
                }
            }
        });

        assert_eq!(mgr.apply_options_from_settings(&settings).unwrap(), 3);
        let options = mgr.plugin_options("configured");
        assert_eq!(options["TOKEN"], "from-settings");
        assert_eq!(options["PORT"], "8080");
        assert_eq!(options["DEBUG"], "true");
    }

    #[test]
    fn a_non_scalar_setting_is_an_error_not_a_silent_drop() {
        let mgr = PluginManager::new();
        let settings = serde_json::json!({
            "pluginOptions": {
                "configured": {"TOKEN": {"nested": "secret"}}
            }
        });
        let err = mgr.apply_options_from_settings(&settings).unwrap_err();
        assert!(err.contains("TOKEN"), "{err}");
    }

    // ── marketplace-name installs ────────────────────────────────────────

    #[tokio::test]
    async fn installing_by_name_without_a_marketplace_says_what_is_missing() {
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();

        let err = mgr
            .install_plugin_by_name("some-plugin", install_dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("no marketplace is registered"), "{err}");
    }

    #[tokio::test]
    async fn installing_by_name_uses_a_registered_marketplace() {
        let install_dir = tempfile::tempdir().unwrap();
        let mgr = PluginManager::new();
        // Port 1 refuses immediately; the point is that the registered
        // marketplace was the one asked, not that a download happened.
        mgr.register_marketplace("main".into(), "https://127.0.0.1:1".into())
            .unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mgr.install_plugin_by_name("some-plugin", install_dir.path()),
        )
        .await
        .expect("the marketplace download attempt timed out");

        let err = result.unwrap_err();
        assert!(
            err.contains("failed to download plugin 'some-plugin'"),
            "{err}"
        );
    }
}
