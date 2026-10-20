//! Hook execution engine — configuration, matching, and subprocess execution.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin};

use crate::action::{
    AGENT_DEFAULT_MODEL, AGENT_DEFAULT_TIMEOUT_MS, ActionPrompt, HookActionRunner, McpToolHook,
    PROMPT_DEFAULT_TIMEOUT_MS, interpolate_tool_input, parse_answer, substitute_arguments,
};
use crate::events::{HookAction, HookError, HookEvent, HookResult};
use crate::sandbox::HookSandboxConfig;
use crate::stream::{HookMessage, HookRequestHandler, HookStreamEvent};
use crate::{sandbox, stream};
use coderex_core::hook_executor::{HookExecutor, HookFireResult};

/// The largest single stdout line the streaming protocol accepts.
const MAX_HOOK_LINE_BYTES: usize = 1024 * 1024;
/// The largest amount of stdout the host will hold for one hook.
const MAX_HOOK_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// The largest number of progress events kept for one hook.
const MAX_HOOK_EVENTS: usize = 1024;
/// The largest amount of stderr kept; the rest is drained and discarded.
const MAX_HOOK_STDERR_BYTES: usize = 1024 * 1024;
/// How long before the hard timeout the cancel message is sent.
const CANCEL_LEAD_MS: u64 = 50;

// ─── Hook Configuration ─────────────────────────────────────────────────────

/// Which trust tier a hook belongs to.
///
/// Hooks are matched in tier order — managed first, then plugin, then core —
/// and only priority breaks ties *within* a tier. The tier is a security
/// property, not cosmetics: the chain order decides which hook gets to speak
/// first, and a later tier cannot pre-empt an earlier one.
///
/// A hook's tier is assigned by the code that loads it, from *where the
/// settings came from* — never from a field a repository can write. A
/// repository-controlled `settings.json` can therefore never promote one of its
/// hooks into the `Managed` tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTier {
    /// Managed policy hooks — highest trust, run first. Loaded only from an
    /// administrator-controlled source.
    Managed,
    /// Plugin-provided hooks.
    Plugin,
    /// Built-in and user/project settings hooks — the default tier.
    #[default]
    Core,
}

/// What kind of action a hook takes when it fires.
///
/// All five kinds the reference has are implemented. `command` and `http` are
/// self-contained; `prompt`, `agent`, and `mcp_tool` need a client the hooks
/// crate does not own, so they run through a
/// [`HookActionRunner`] the session installs — and are a failure, not a skip,
/// when none is installed (`HookError::MissingActionRunner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    /// Run a shell command.
    #[default]
    Command,
    /// Call an HTTP endpoint.
    Http,
    /// Ask a model one question; its text answer is the hook's answer.
    Prompt,
    /// Call an MCP tool; its result is the hook's answer.
    McpTool,
    /// Ask a model as a verifier, with a cheap model and a longer timeout.
    Agent,
}

impl HookType {
    /// The wire name used in settings, matching Claude's `type` values.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Http => "http",
            Self::Prompt => "prompt",
            Self::McpTool => "mcp_tool",
            Self::Agent => "agent",
        }
    }
}

/// The target of an `http` hook.
///
/// The hook input is POSTed to [`url`](Self::url), and the response body is
/// read as the hook's answer — the same JSON a `command` hook prints on stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpHook {
    /// Where the hook input is POSTed.
    ///
    /// Only an absolute `http`/`https` URL: the body is the conversation, so
    /// the destination has to be named outright rather than resolved through a
    /// relative path or a scheme that points at a local file.
    pub url: String,
    /// Extra request headers.
    ///
    /// A value may reference an environment variable as `$NAME` or `${NAME}`,
    /// but only a name [`allowed_env_vars`](Self::allowed_env_vars) grants.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// The environment variables a header value may reference.
    ///
    /// A configuration that references anything else is refused at load, so a
    /// hook cannot read out a variable simply by naming it.
    #[serde(default)]
    pub allowed_env_vars: Vec<String>,
    /// Refuse an endpoint whose host resolves to localhost or a private range.
    ///
    /// Off by default: a local hook endpoint is an ordinary thing to want. On,
    /// it stops a configuration from reaching a cloud metadata service or an
    /// internal host through the hook's request.
    #[serde(default)]
    pub block_private_hosts: bool,
}

impl HttpHook {
    /// Check the URL and the header references.
    ///
    /// # Errors
    ///
    /// Returns the reason the hook cannot be used as configured.
    pub fn validate(&self) -> Result<(), HookError> {
        coderex_core::hook_security::UrlValidator::new()
            .validate(&self.url)
            .map_err(|e| HookError::InvalidHttpHook(e.to_string()))?;

        for (header, value) in &self.headers {
            for name in coderex_core::hook_security::env_var_references(value) {
                if !self.allowed_env_vars.contains(&name) {
                    return Err(HookError::InvalidHttpHook(format!(
                        "header '{header}' references ${name}, which is not in allowedEnvVars"
                    )));
                }
            }
        }

        Ok(())
    }
}

/// Configuration for a single hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Event(s) this hook responds to.
    pub event: String,
    /// What kind of action this hook takes.
    ///
    /// Serialized as `type`, matching Claude's discriminator. Absent means
    /// [`HookType::Command`].
    #[serde(rename = "type", default)]
    pub kind: HookType,
    /// Command to execute (required for [`HookType::Command`]).
    ///
    /// Empty for an [`HookType::Http`] hook — the loader fills in exactly one
    /// of the two targets per kind. [`label`](Self::label) is what a log line
    /// should use.
    pub command: String,
    /// Where an `http` hook sends its input (required for [`HookType::Http`]).
    #[serde(default)]
    pub http: Option<HttpHook>,
    /// The prompt a `prompt` or `agent` hook sends (required for both kinds).
    ///
    /// `$ARGUMENTS` (or `${ARGUMENTS}`) is replaced with the hook input JSON
    /// before the query is made.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The model a `prompt` or `agent` hook uses.
    ///
    /// `None` for a `prompt` hook means "the session's default". An `agent`
    /// hook always carries one — the loader fills in
    /// [`AGENT_DEFAULT_MODEL`](crate::action::AGENT_DEFAULT_MODEL) when the
    /// configuration names none.
    #[serde(default)]
    pub model: Option<String>,
    /// The MCP tool an `mcp_tool` hook calls (required for [`HookType::McpTool`]).
    #[serde(default)]
    pub mcp: Option<McpToolHook>,
    /// Optional environment variables to inject.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional working directory for the hook.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Whether this is an internal hook (not user-configured).
    #[serde(default)]
    pub internal: bool,
    /// Timeout in milliseconds (default 5000).
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Execution priority (higher runs first, default 0).
    #[serde(default)]
    pub priority: u32,
    /// If true, execute but ignore stop decisions (for testing).
    #[serde(default)]
    pub dry_run: bool,
    /// Whether this hook speaks the bidirectional streaming protocol.
    ///
    /// `false` (the default) is the historical one-way contract: the host
    /// writes the input and **closes stdin**, so a hook that reads stdin to
    /// EOF sees the end. `true` keeps stdin open for `response` and `cancel`
    /// messages; a hook that emits `request` needs it.
    #[serde(default)]
    pub stream: bool,
    /// Maximum CPU seconds for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    /// Maximum memory (MB) for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Maximum output file size (MB) for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
    /// OS-level filesystem and network confinement for this hook.
    ///
    /// `None` means the default policy: read the workspace, home/config and
    /// system roots; write the workspace and temp directories; no network.
    /// The sandbox is applied in addition to the resource limits above and
    /// only to `command` hooks, since it is a subprocess concern.
    ///
    /// A per-hook policy can extend the paths, permit the network, or opt out
    /// with `enabled: false`. On a platform that cannot enforce confinement a
    /// gating hook fails closed unless it opts out; see
    /// [`HOOK_SANDBOX_ENV`](crate::HOOK_SANDBOX_ENV).
    #[serde(default)]
    pub sandbox: Option<HookSandboxConfig>,
    /// Trust tier — assigned by the loader, never read from settings.
    ///
    /// `#[serde(skip)]` is deliberate: a settings file that writes `tier` must
    /// not be able to promote its own hook into a higher-trust tier.
    #[serde(skip, default)]
    pub tier: HookTier,
}

const fn default_timeout() -> u64 {
    5000
}

impl HookConfig {
    /// What this hook should be called in a log line.
    ///
    /// A hook names itself by the target of its kind — its command, an `http`
    /// hook's URL, an `mcp_tool` hook's tool, a `prompt`/`agent` hook's prompt.
    /// `command` is empty for every kind but `command`, and a log line reading
    /// `hook_command=""` says nothing about which hook ran.
    #[must_use]
    pub fn label(&self) -> &str {
        if let Some(http) = self.http.as_ref() {
            return &http.url;
        }
        if !self.command.is_empty() {
            return &self.command;
        }
        if let Some(mcp) = self.mcp.as_ref() {
            return &mcp.tool;
        }
        self.prompt.as_deref().unwrap_or_default()
    }
}

// ─── Hook Settings (from settings.json) ─────────────────────────────────────

/// Parsed hook settings from the `hooks` field in settings.json.
#[derive(Debug, Clone, Deserialize)]
pub struct HookSettings {
    /// List of hook configurations.
    #[serde(default)]
    pub hooks: Vec<RawHookConfig>,
}

/// Extract the raw hook entries from either accepted document shape.
///
/// `Settings.hooks` in `coderex-config` holds the bare array — the value under
/// the `hooks` key of `settings.json` — while a whole settings object is
/// `{"hooks": [...]}`. The install API accepts either so the caller does not
/// have to know which it has.
///
/// The entries stay as [`serde_json::Value`] on purpose: [`parse_settings_hooks`]
/// deserializes them one at a time so one malformed entry can be skipped
/// without discarding the rest of the document. Only a document that is
/// neither shape is a structural error.
///
/// [`parse_settings_hooks`]: HookRegistry::parse_settings_hooks
fn settings_hook_entries(value: &serde_json::Value) -> Result<Vec<serde_json::Value>, HookError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    match value {
        // `[...]` — the bare array, as `Settings.hooks` holds it.
        serde_json::Value::Array(entries) => Ok(entries.clone()),
        // `{"hooks": [...]}` — a whole settings object. An object with no
        // `hooks` key contributes nothing, matching `HookSettings`'s
        // `#[serde(default)]`.
        serde_json::Value::Object(map) => match map.get("hooks") {
            None => Ok(Vec::new()),
            Some(hooks) => Ok(serde_json::from_value::<Vec<serde_json::Value>>(
                hooks.clone(),
            )?),
        },
        // Neither shape: a scalar or boolean cannot be a hooks document. This
        // is a top-level shape error, not an entry-level one.
        _ => Err(HookError::JsonError(serde::de::Error::custom(
            "expected a hooks array or an object with a 'hooks' array",
        ))),
    }
}

/// Raw hook config as it appears in settings.json (more flexible field types).
#[derive(Debug, Clone, Deserialize)]
pub struct RawHookConfig {
    /// Event name or pattern.
    pub event: String,
    /// Hook kind (`type` in settings). Absent means `command`.
    #[serde(rename = "type", default)]
    pub kind: HookType,
    /// Command to run (required for the `command` kind).
    #[serde(default)]
    pub command: Option<String>,
    /// Where to POST the hook input (required for the `http` kind).
    #[serde(default)]
    pub url: Option<String>,
    /// Extra request headers for an `http` hook.
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// The environment variables a header value may reference.
    #[serde(rename = "allowedEnvVars", default)]
    pub allowed_env_vars: Option<Vec<String>>,
    /// Refuse an `http` endpoint on localhost or a private range.
    #[serde(default)]
    pub block_private_hosts: Option<bool>,
    /// The prompt a `prompt` or `agent` hook sends.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The model a `prompt` or `agent` hook uses.
    #[serde(default)]
    pub model: Option<String>,
    /// The MCP server an `mcp_tool` hook calls.
    #[serde(default)]
    pub server: Option<String>,
    /// The MCP tool an `mcp_tool` hook calls.
    #[serde(default)]
    pub tool: Option<String>,
    /// The tool input an `mcp_tool` hook passes.
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    /// Accepted but not implemented — see `warn_about_unimplemented_fields`.
    #[serde(rename = "if", default)]
    pub if_condition: Option<serde_json::Value>,
    /// Accepted but not implemented.
    #[serde(default)]
    pub once: Option<bool>,
    /// Accepted but not implemented.
    #[serde(rename = "statusMessage", default)]
    pub status_message: Option<String>,
    /// Accepted but not implemented.
    #[serde(rename = "continueOnBlock", default)]
    pub continue_on_block: Option<bool>,
    /// Environment variables to inject.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Whether internal.
    #[serde(default)]
    pub internal: Option<bool>,
    /// Timeout in ms.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Execution priority.
    #[serde(default)]
    pub priority: Option<u32>,
    /// Dry-run mode.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Whether this hook speaks the bidirectional streaming protocol.
    #[serde(default)]
    pub stream: Option<bool>,
    /// Max CPU seconds.
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    /// Max memory MB.
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Max file size MB.
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
    /// OS sandbox policy, or `false` to opt this hook out.
    #[serde(default)]
    pub sandbox: Option<RawSandboxSetting>,
}

/// The two shapes the `sandbox` key accepts.
///
/// `false` is the short opt-out; an object is a policy. `true` means the
/// default policy. Anything else — including an unknown key inside the object
/// — is refused at load rather than silently ignored; that entry is skipped
/// and the rest of the source still loads.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawSandboxSetting {
    /// `"sandbox": false` (or `true`).
    Toggle(bool),
    /// `"sandbox": { ... }`.
    Config(HookSandboxConfig),
}

impl RawSandboxSetting {
    fn into_config(self) -> HookSandboxConfig {
        match self {
            Self::Toggle(enabled) => HookSandboxConfig {
                enabled,
                ..HookSandboxConfig::default()
            },
            Self::Config(config) => config,
        }
    }
}

/// The fields every hook kind shares, taken from its raw configuration.
///
/// The kind-specific fields (`command`, `http`, `prompt`/`model`, `mcp`) are
/// filled in by the caller; the tier is assigned by the caller too, from where
/// the settings came from.
fn from_raw(raw: &RawHookConfig, timeout_ms: u64) -> HookConfig {
    HookConfig {
        event: raw.event.clone(),
        kind: raw.kind,
        command: String::new(),
        http: None,
        prompt: None,
        model: None,
        mcp: None,
        env: raw.env.clone().unwrap_or_default(),
        cwd: raw.cwd.clone(),
        internal: raw.internal.unwrap_or(false),
        timeout_ms,
        priority: raw.priority.unwrap_or(0),
        dry_run: raw.dry_run.unwrap_or(false),
        stream: raw.stream.unwrap_or(false),
        max_cpu_seconds: raw.max_cpu_seconds,
        max_memory_mb: raw.max_memory_mb,
        max_file_size_mb: raw.max_file_size_mb,
        sandbox: raw.sandbox.clone().map(RawSandboxSetting::into_config),
        tier: HookTier::Core,
    }
}

/// Say which configured fields this build accepts but does not act on.
///
/// `if`, `once`, `statusMessage`, and `continueOnBlock` are a separate axis
/// from the hook kind: no kind implements them, including `command` and
/// `http`. They are accepted so a settings file written for the reference does
/// not fail to load, but a person reading the output should not have to guess
/// whether a configured `if` filtered anything.
fn warn_about_unimplemented_fields(raw: &RawHookConfig) {
    let mut declared = Vec::new();
    if raw.if_condition.is_some() {
        declared.push("if");
    }
    if raw.once.is_some() {
        declared.push("once");
    }
    if raw.status_message.is_some() {
        declared.push("statusMessage");
    }
    if raw.continue_on_block.is_some() {
        declared.push("continueOnBlock");
    }
    if !declared.is_empty() {
        tracing::warn!(
            event = %raw.event,
            fields = ?declared,
            "hook sets fields CodeRex accepts but does not implement — they have no effect"
        );
    }
}

// ─── Hook Execution Result ──────────────────────────────────────────────────

/// Result from executing a hook, including subprocess output.
#[derive(Debug, Clone)]
pub struct HookExecutionResult {
    /// Parsed hook result from stdout.
    pub hook_result: HookResult,
    /// Standard output from the hook subprocess (the protocol lines it wrote).
    pub stdout: String,
    /// Standard error from the hook subprocess.
    pub stderr: String,
    /// Exit code from the hook subprocess.
    pub exit_code: i32,
    /// Progress events the hook emitted before its answer, in order.
    pub events: Vec<HookStreamEvent>,
}

// ─── Hook Input (serialized to stdin) ───────────────────────────────────────

/// Input sent to a hook subprocess via stdin as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct HookInput {
    /// The event type that triggered this hook.
    pub event: String,
    /// Session ID.
    pub session_id: Option<String>,
    /// Current user input.
    pub input: Option<String>,
    /// Additional context about the event.
    pub context: Option<String>,
}

// ─── Hook Registry ──────────────────────────────────────────────────────────

/// Registry of configured hooks.
pub struct HookRegistry {
    hooks: Vec<HookConfig>,
    /// Supplied by the session that owns a model client or the MCP registry;
    /// see [`HookActionRunner`].
    action_runner: Option<Arc<dyn HookActionRunner>>,
    /// Supplied by the session when it can answer a hook's inbound request;
    /// without one the request is refused with `-32601`.
    request_handler: Option<Arc<dyn HookRequestHandler>>,
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("hooks", &self.hooks)
            .field("action_runner", &self.action_runner.is_some())
            .field("request_handler", &self.request_handler.is_some())
            .finish()
    }
}

impl HookRegistry {
    /// Create an empty hook registry.
    ///
    /// No action runner is installed, so `prompt`, `agent`, and `mcp_tool`
    /// hooks will fail when they fire until one is
    /// [set](Self::set_action_runner).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hooks: Vec::new(),
            action_runner: None,
            request_handler: None,
        }
    }

    /// Install the runner a `prompt`, `agent`, or `mcp_tool` hook needs.
    ///
    /// The hooks crate cannot reach a model client or the MCP server registry,
    /// so the session that owns them supplies one implementation — the same
    /// injection shape as `coderex_tools::install_rule_exemptions`. This must
    /// be called before [`into_executor`](Self::into_executor), since that
    /// consumes the registry.
    ///
    /// Without a runner the three kinds are still **loaded**, and then
    /// **fail** when they fire (`HookError::MissingActionRunner`) — a gating
    /// event denies rather than quietly passing. That is deliberate: a
    /// configured gate this session cannot run is a gate that must not read as
    /// an allow.
    pub fn set_action_runner(&mut self, runner: Arc<dyn HookActionRunner>) {
        self.action_runner = Some(runner);
    }

    /// Install the handler for requests a hook makes while it runs.
    ///
    /// The streaming protocol lets a `command` hook ask the host a question
    /// (`{"type":"request",...}`) and wait for an answer on stdin. Without a
    /// handler every such request is refused with `-32601`, which is an
    /// answer — the hook is never left waiting.
    pub fn set_request_handler(&mut self, handler: Arc<dyn HookRequestHandler>) {
        self.request_handler = Some(handler);
    }

    /// The installed request handler, if any.
    #[must_use]
    pub fn request_handler(&self) -> Option<Arc<dyn HookRequestHandler>> {
        self.request_handler.clone()
    }

    /// The runner installed on this registry, if any.
    #[must_use]
    pub fn action_runner(&self) -> Option<Arc<dyn HookActionRunner>> {
        self.action_runner.clone()
    }

    /// Load hooks from a settings.json value, assigning every hook the given
    /// trust tier.
    ///
    /// Every kind is loaded: `command`, `http`, `prompt`, `agent`, and
    /// `mcp_tool`. The last three need a
    /// [`HookActionRunner`] installed by the session (see
    /// [`set_action_runner`](Self::set_action_runner)); without one they load
    /// and then **fail when they fire**, they are not skipped.
    ///
    /// One invalid entry does **not** fail the load: it is skipped with a
    /// warning and the remaining entries in the document are kept. A hook
    /// missing the field its kind requires (no `command`, no `url`, no
    /// `prompt`, no `server`/`tool`) is one such entry. Only a document that
    /// is not a hooks array — or an object whose `hooks` value is not an array
    /// — is a structural error and returns `Err`.
    pub fn load_from_settings_with_tier(
        value: &serde_json::Value,
        tier: HookTier,
    ) -> Result<Self, HookError> {
        let hooks = Self::parse_settings_hooks(value, tier)?;
        Self::warn_about_inert_hooks(&hooks);
        Ok(Self {
            hooks,
            action_runner: None,
            request_handler: None,
        })
    }

    /// Parse one settings document into hook configs, tagging every hook with
    /// `tier`.
    ///
    /// Entries are deserialized and validated one at a time. An entry that
    /// fails either step is logged and skipped, so a mistyped hook in a
    /// repository-controlled document cannot remove the rest of the chain.
    /// Only a structural error in the document itself (neither a bare hooks
    /// array nor an object with a `hooks` array) is returned as `Err`.
    ///
    /// The tier comes from the caller, never from the document: `HookConfig`'s
    /// `tier` field is `#[serde(skip)]`, so a `"tier": "managed"` written into
    /// a repository's settings is simply an unknown field and is ignored.
    fn parse_settings_hooks(
        value: &serde_json::Value,
        tier: HookTier,
    ) -> Result<Vec<HookConfig>, HookError> {
        let raw_hooks = settings_hook_entries(value)?;
        let mut hooks = Vec::with_capacity(raw_hooks.len());
        for raw_value in raw_hooks {
            let raw = match serde_json::from_value::<RawHookConfig>(raw_value) {
                Ok(raw) => raw,
                Err(error) => {
                    tracing::warn!(
                        tier = ?tier,
                        %error,
                        "skipping a malformed hook entry; the rest of this settings source still loads"
                    );
                    continue;
                }
            };
            match Self::hook_from_raw(&raw, tier) {
                Ok(config) => hooks.push(config),
                Err(error) => tracing::warn!(
                    tier = ?tier,
                    event = %raw.event,
                    %error,
                    "skipping an invalid hook entry; the rest of this settings source still loads"
                ),
            }
        }
        Ok(hooks)
    }

    /// Turn one deserialized entry into a [`HookConfig`], or explain why it
    /// cannot be used.
    ///
    /// The caller skips an `Err` rather than failing the whole document.
    fn hook_from_raw(raw: &RawHookConfig, tier: HookTier) -> Result<HookConfig, HookError> {
        warn_about_unimplemented_fields(raw);
        let mut config = match raw.kind {
            HookType::Command => {
                let Some(command) = raw.command.clone() else {
                    return Err(HookError::InvalidCommand(format!(
                        "hook for event '{}' is type 'command' but has no 'command' field",
                        raw.event
                    )));
                };
                HookConfig {
                    command,
                    ..from_raw(raw, raw.timeout_ms.unwrap_or_else(default_timeout))
                }
            }
            HookType::Http => {
                let Some(url) = raw.url.clone() else {
                    return Err(HookError::InvalidHttpHook(format!(
                        "hook for event '{}' is type 'http' but has no 'url' field",
                        raw.event
                    )));
                };
                let http = HttpHook {
                    url,
                    headers: raw.headers.clone().unwrap_or_default(),
                    allowed_env_vars: raw.allowed_env_vars.clone().unwrap_or_default(),
                    block_private_hosts: raw.block_private_hosts.unwrap_or(false),
                };
                http.validate()?;
                HookConfig {
                    http: Some(http),
                    ..from_raw(raw, raw.timeout_ms.unwrap_or_else(default_timeout))
                }
            }
            HookType::Prompt | HookType::Agent => {
                let Some(prompt) = raw.prompt.clone() else {
                    return Err(HookError::InvalidPromptHook(format!(
                        "hook for event '{}' is type '{}' but has no 'prompt' field",
                        raw.event,
                        raw.kind.as_str()
                    )));
                };
                // The two kinds differ on purpose: an `agent` hook is a
                // verifier, so it gets the cheap model and a longer
                // timeout; a `prompt` hook is one question and keeps the
                // session's model.
                let agent = raw.kind == HookType::Agent;
                let timeout_ms = raw.timeout_ms.unwrap_or(if agent {
                    AGENT_DEFAULT_TIMEOUT_MS
                } else {
                    PROMPT_DEFAULT_TIMEOUT_MS
                });
                let model = if agent {
                    Some(
                        raw.model
                            .clone()
                            .unwrap_or_else(|| AGENT_DEFAULT_MODEL.to_string()),
                    )
                } else {
                    raw.model.clone()
                };
                HookConfig {
                    prompt: Some(prompt),
                    model,
                    ..from_raw(raw, timeout_ms)
                }
            }
            HookType::McpTool => {
                let Some(server) = raw.server.clone() else {
                    return Err(HookError::InvalidMcpToolHook(format!(
                        "hook for event '{}' is type 'mcp_tool' but has no 'server' field",
                        raw.event
                    )));
                };
                let Some(tool) = raw.tool.clone() else {
                    return Err(HookError::InvalidMcpToolHook(format!(
                        "hook for event '{}' is type 'mcp_tool' but has no 'tool' field",
                        raw.event
                    )));
                };
                let mcp = McpToolHook {
                    server,
                    tool,
                    input: raw
                        .input
                        .clone()
                        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
                };
                HookConfig {
                    mcp: Some(mcp),
                    ..from_raw(raw, raw.timeout_ms.unwrap_or_else(default_timeout))
                }
            }
        };
        if let Some(sandbox) = &config.sandbox {
            sandbox.validate()?;
        }
        config.tier = tier;
        Ok(config)
    }

    /// Load hooks from a settings.json value.
    ///
    /// Every hook is assigned [`HookTier::Core`]; use
    /// [`load_from_settings_with_tier`](Self::load_from_settings_with_tier) to
    /// load from a managed or plugin source.
    pub fn load_from_settings(value: &serde_json::Value) -> Result<Self, HookError> {
        Self::load_from_settings_with_tier(value, HookTier::Core)
    }

    /// Install API: build a registry from the `hooks` value of `Settings`.
    ///
    /// This is the one-line entry point for the CLI. `Settings.hooks` is the raw
    /// JSON value; pass it through here, then
    /// [`into_executor`](Self::into_executor):
    ///
    /// ```ignore
    /// let registry = HookRegistry::from_settings_json(
    ///     settings.hooks.as_ref().unwrap_or(&serde_json::Value::Null),
    /// )?;
    /// engine.set_hooks(registry.into_executor());
    /// ```
    ///
    /// Every hook is assigned [`HookTier::Core`]. Hooks from an
    /// administrator-controlled source must use
    /// [`from_settings_json_with_tier`](Self::from_settings_json_with_tier) so
    /// they can run ahead of repository-controlled hooks.
    pub fn from_settings_json(value: &serde_json::Value) -> Result<Self, HookError> {
        Self::load_from_settings_with_tier(value, HookTier::Core)
    }

    /// Install API: like [`from_settings_json`](Self::from_settings_json), but
    /// for a managed or plugin source.
    pub fn from_settings_json_with_tier(
        value: &serde_json::Value,
        tier: HookTier,
    ) -> Result<Self, HookError> {
        Self::load_from_settings_with_tier(value, tier)
    }

    /// Install API: build one registry from several settings documents, each
    /// with the tier of the source it came from.
    ///
    /// This is what a session should use: managed hooks must run at
    /// [`HookTier::Managed`] and must **not** also be present in the core
    /// document (loading the merged `Settings.hooks` as `Core` would both lose
    /// the ordering guarantee and run a managed hook twice). Sources are given
    /// as `(tier, document)` pairs and may be `serde_json::Value::Null`, which
    /// contributes nothing.
    ///
    /// Sources are parsed independently, so a failure in one cannot discard
    /// the hooks from the others. This matters most for a managed source: a
    /// repository can write its own settings, and a single malformed project
    /// hook must not remove administrator-controlled gating hooks
    /// (`PreToolUse`/`UserPromptSubmit`). A structural error in a source is
    /// therefore logged — at `error` level for a managed source, `warn` for
    /// the rest — and the remaining sources still load. The alternative,
    /// dropping every hook, is strictly worse: the invalid source contributes
    /// nothing either way, and the valid ones may be the gates enforcing
    /// policy.
    ///
    /// Entry-level errors do not even reach this point: individual malformed
    /// entries within a source are skipped by [`parse_settings_hooks`], so the
    /// valid entries in that same source still load. The `Result` return is
    /// kept for compatibility, but per-source structural errors are isolated
    /// here and logged rather than returned.
    ///
    /// The resulting registry sorts by tier first, then priority, exactly like
    /// one built with [`register`](Self::register).
    ///
    /// [`parse_settings_hooks`]: Self::parse_settings_hooks
    pub fn from_settings_sources<'a, I>(sources: I) -> Result<Self, HookError>
    where
        I: IntoIterator<Item = (HookTier, &'a serde_json::Value)>,
    {
        let mut hooks = Vec::new();
        for (tier, value) in sources {
            match Self::parse_settings_hooks(value, tier) {
                Ok(parsed) => hooks.extend(parsed),
                Err(error) => match tier {
                    // A managed source carries policy the user cannot repair;
                    // losing it is an operator-visible problem. Say so at
                    // error level, but keep every other source's hooks.
                    HookTier::Managed => tracing::error!(
                        tier = ?tier,
                        %error,
                        "hook source is structurally invalid; continuing without it"
                    ),
                    HookTier::Plugin | HookTier::Core => tracing::warn!(
                        tier = ?tier,
                        %error,
                        "hook source is structurally invalid; continuing without it"
                    ),
                },
            }
        }
        Self::warn_about_inert_hooks(&hooks);
        Ok(Self {
            hooks,
            action_runner: None,
            request_handler: None,
        })
    }

    /// Install API: turn the registry into the boxed executor the engine takes,
    /// so `engine.set_hooks(registry.into_executor())` is one line.
    #[must_use]
    pub fn into_executor(self) -> Box<dyn HookExecutor + Send + Sync> {
        Box::new(self)
    }

    /// Report hooks whose event nothing ever fires.
    ///
    /// A hook on an event CodeRex does not fire matches nothing and runs
    /// never. That is indistinguishable from a working hook until someone
    /// notices it did not run, so it is said out loud at load.
    fn warn_about_inert_hooks(hooks: &[HookConfig]) {
        for hook in hooks {
            if hook.event == "*" {
                continue;
            }
            match hook.event.parse::<HookEvent>() {
                Ok(event) if event.is_triggered() => {}
                Ok(event) => tracing::warn!(
                    event = %event,
                    command = %hook.command,
                    "hook configured for an event CodeRex does not fire — it will never run"
                ),
                Err(unknown) => tracing::warn!(
                    event = %unknown,
                    command = %hook.command,
                    "hook configured for an unknown event — it will never run"
                ),
            }
        }
    }

    /// Register a hook configuration, in the tier the config carries.
    pub fn register(&mut self, config: HookConfig) {
        self.hooks.push(config);
    }

    /// Register a hook configuration in an explicit tier, overriding the tier
    /// the config carries.
    pub fn register_in_tier(&mut self, tier: HookTier, mut config: HookConfig) {
        config.tier = tier;
        self.hooks.push(config);
    }

    /// Find hooks matching the given event, in chain order.
    ///
    /// Chain order is tier first — managed, then plugin, then core — and
    /// priority (highest first) only within a tier. Insertion order breaks
    /// remaining ties. Tier order is a security property: a managed hook runs
    /// ahead of a repository-controlled one no matter what priority the
    /// repository gives itself.
    #[must_use]
    pub fn matches_for_event(&self, event: &HookEvent) -> Vec<&HookConfig> {
        self.matches_for_event_name(event.as_str())
    }

    /// Find hooks matching the given event name string, in chain order.
    ///
    /// Both sides are parsed as a [`HookEvent`] before comparing, so a
    /// configured historical alias such as `PostToolUseFail` matches the
    /// canonical `PostToolUseFailure` that is fired. Names that do not parse
    /// match nothing (except `"*"`): matching raw strings would make every
    /// alias and every typo silently inert.
    fn matches_for_event_name(&self, event_name: &str) -> Vec<&HookConfig> {
        let fired = event_name.parse::<HookEvent>().ok();
        let mut hooks: Vec<_> = self
            .hooks
            .iter()
            .filter(|h| {
                if h.event == "*" {
                    return true;
                }
                match (h.event.parse::<HookEvent>(), fired) {
                    (Ok(configured), Some(fired)) => configured == fired,
                    _ => false,
                }
            })
            .collect();
        // Stable sort: tier ascending, then priority descending. Equal keys
        // keep registration order.
        hooks.sort_by_key(|h| (h.tier, std::cmp::Reverse(h.priority)));
        hooks
    }

    /// Execute matching hooks synchronously for a named event.
    async fn execute_sync_for_event(
        &self,
        event_name: &str,
        input: &HookInput,
    ) -> Vec<Result<HookExecutionResult, HookError>> {
        let matching = self.matches_for_event_name(event_name);
        let mut results = Vec::new();
        for hook in matching {
            let result = execute_hook_with_timeout(
                hook,
                input,
                self.action_runner.as_deref(),
                self.request_handler.as_deref(),
            )
            .await;
            results.push(result);
        }
        results
    }

    /// Execute matching hooks synchronously (sequential, collect results).
    pub async fn execute_sync(
        &self,
        event: &HookEvent,
        input: &HookInput,
    ) -> Vec<Result<HookExecutionResult, HookError>> {
        let matching = self.matches_for_event(event);
        let mut results = Vec::new();
        for hook in matching {
            let result = execute_hook_with_timeout(
                hook,
                input,
                self.action_runner.as_deref(),
                self.request_handler.as_deref(),
            )
            .await;
            results.push(result);
        }
        results
    }

    /// Execute matching hooks asynchronously (spawn tasks, collect results).
    pub async fn execute_async(
        &self,
        event: &HookEvent,
        input: &HookInput,
    ) -> Vec<Result<HookExecutionResult, HookError>> {
        let matching: Vec<_> = self.matches_for_event(event);
        let mut handles = Vec::new();
        for hook in matching {
            let hook = hook.clone();
            let input = input.clone();
            let runner = self.action_runner.clone();
            let requests = self.request_handler.clone();
            let handle = tokio::spawn(async move {
                execute_hook_with_timeout(&hook, &input, runner.as_deref(), requests.as_deref())
                    .await
            });
            handles.push(handle);
        }
        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(Err(HookError::SpawnFailed(e.to_string()))),
            }
        }
        results
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl HookExecutor for HookRegistry {
    async fn fire_hook(
        &self,
        event_name: &str,
        input: &str,
        context: &HashMap<String, String>,
    ) -> HookFireResult {
        // Build hook input
        let hook_input = HookInput {
            event: event_name.to_string(),
            session_id: context.get("session_id").cloned(),
            input: Some(input.to_string()),
            context: None,
        };

        // Execute matching hooks for this event
        let results = self.execute_sync_for_event(event_name, &hook_input).await;

        // Whether a failure here can deny. Only the events whose firing site
        // reads `stop` as a gate can (see `HookEvent::is_gating`); an unknown
        // event name fires nowhere, so it cannot.
        let gating = event_name
            .parse::<HookEvent>()
            .is_ok_and(HookEvent::is_gating);

        let mut combined = HookFireResult::default();
        for exec_result in results {
            match exec_result {
                Ok(r) => {
                    if matches!(r.hook_result.action, HookAction::Stop) {
                        combined.stop = true;
                    }
                    if r.hook_result.updated_input.is_some() {
                        combined.updated_input = r.hook_result.updated_input;
                    }
                }
                Err(e) => {
                    // Fail closed: a hook that could not answer is not an
                    // allow. Record the failure, and on a gating event deny.
                    combined.failed = true;
                    combined.errors.push(format!("{e}"));
                }
            }
        }
        if combined.failed && gating {
            combined.stop = true;
        }
        combined
    }
}

// ─── Hook Execution ─────────────────────────────────────────────────────────

/// Execute a single hook with timeout.
async fn execute_hook_with_timeout(
    hook: &HookConfig,
    input: &HookInput,
    action_runner: Option<&dyn HookActionRunner>,
    request_handler: Option<&dyn HookRequestHandler>,
) -> Result<HookExecutionResult, HookError> {
    let start = std::time::Instant::now();

    if hook.dry_run {
        tracing::info!(
            hook = hook.label(),
            event = input.event,
            "Hook would execute (dry-run)"
        );
    }

    let timeout_ms = hook.timeout_ms;
    // Leave a sliver of the timeout for the cancel message: writing it and
    // then killing is a best-effort courtesy, and it can only happen before
    // the hard timeout does.
    let stream_deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(timeout_ms.saturating_sub(CANCEL_LEAD_MS).max(1));
    let execution = async {
        match hook.kind {
            HookType::Command => execute_hook(hook, input, request_handler, stream_deadline).await,
            HookType::Http => execute_http_hook(hook, input).await,
            HookType::Prompt => execute_prompt_hook(hook, input, action_runner, false).await,
            HookType::Agent => execute_prompt_hook(hook, input, action_runner, true).await,
            HookType::McpTool => execute_mcp_tool_hook(hook, input, action_runner).await,
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), execution)
        .await
        .unwrap_or(Err(HookError::Timeout(timeout_ms)));

    let elapsed = start.elapsed();

    match &result {
        Ok(exec_result) => {
            tracing::info!(
                hook = hook.label(),
                event = input.event,
                duration_ms = elapsed.as_millis(),
                exit_code = exec_result.exit_code,
                stop = matches!(exec_result.hook_result.action, HookAction::Stop),
                "Hook executed"
            );
        }
        Err(e) => {
            tracing::warn!(
                hook = hook.label(),
                event = input.event,
                duration_ms = elapsed.as_millis(),
                error = ?e,
                "Hook failed"
            );
        }
    }

    // In dry-run mode, suppress stop actions so they don't affect execution.
    if hook.dry_run {
        if let Ok(mut exec_result) = result {
            if matches!(exec_result.hook_result.action, HookAction::Stop) {
                exec_result.hook_result.action = HookAction::Continue;
                exec_result.hook_result.suppress_output = false;
            }
            return Ok(exec_result);
        }
    }

    result
}

/// Execute a single hook subprocess.
///
/// Every way the hook can fail to produce an answer is an `Err`: an
/// undeliverable input, a non-zero exit, an empty stdout, or output that is not
/// a hook result. Only a hook that answered is `Ok`. This is the fail-closed
/// core — [`HookRegistry::fire_hook`] turns the `Err` into a deny on the events
/// that gate.
async fn execute_hook(
    hook: &HookConfig,
    input: &HookInput,
    request_handler: Option<&dyn HookRequestHandler>,
    deadline: tokio::time::Instant,
) -> Result<HookExecutionResult, HookError> {
    let input_json = serde_json::to_string(input)?;
    let env = build_hook_env(hook, input);

    // Confinement is decided before anything is spawned. A gating hook that
    // cannot be confined is a failure (`SandboxUnavailable`), which `fire_hook`
    // turns into a deny; a non-gating one is warned about and runs with
    // resource limits only. The handle must stay alive until the child exits —
    // it owns the macOS profile file and, on Windows, the Job Object.
    let gating = input
        .event
        .parse::<HookEvent>()
        .is_ok_and(HookEvent::is_gating);
    let sandbox_handle = sandbox::enforce(sandbox::plan_for_hook(hook), gating)?;

    let mut command = sandbox::sandboxed_command(&hook.command, sandbox_handle.as_ref());
    if let Some(cwd) = &hook.cwd {
        command.current_dir(cwd);
    }
    command
        .envs(&env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A timeout (or any dropped execution) must not leave the hook
        // running; the timeout future is dropped, not awaited.
        .kill_on_drop(true);

    // Limits are installed before exec, so the hook cannot raise them. On
    // Unix they are inherited by the sandbox wrapper and the shell it execs.
    sandbox::apply_resource_limits(&mut command, hook);

    // On Linux without bubblewrap the filesystem backend is Landlock, which
    // is entered from `pre_exec`; the wrapper path needs no preparation here.
    // A gating hook that cannot install its confinement is denied. A
    // non-gating hook has nowhere to deny, so it is warned about and runs with
    // the resource limits installed above — the same policy `enforce` applies
    // when no backend exists at all.
    if let Some(handle) = &sandbox_handle {
        if let Err(error) = handle.prepare(command.as_std_mut()) {
            if gating {
                return Err(HookError::SandboxUnavailable(format!(
                    "could not install the hook sandbox: {error}"
                )));
            }
            tracing::warn!(
                %error,
                "could not install the hook sandbox; the hook runs with resource limits only"
            );
        }
    }

    // A Linux memory limit is also a cgroup: created before spawn, attached
    // immediately after. Kept alive until the child has been waited for.
    #[cfg(target_os = "linux")]
    let cgroup = sandbox::prepare_cgroup_limits(hook);

    let mut child = command
        .spawn()
        .map_err(|e| HookError::SpawnFailed(e.to_string()))?;

    // Windows finishes the spawn here: attach the AppContainer token, assign
    // the Job Object and resume the suspended child. This is the same handle
    // `prepare` used, so there is one job, not two.
    #[cfg(target_os = "windows")]
    sandbox::finish_windows_spawn(sandbox_handle.as_ref(), &mut child).await?;

    // Windows has no `RLIMIT_FSIZE`; saying nothing would make the accepted
    // field look enforced.
    #[cfg(target_os = "windows")]
    if hook.max_file_size_mb.is_some() {
        tracing::warn!(
            "hook `max_file_size_mb` is not enforced on Windows; use the OS sandbox for \
             file writes"
        );
    }

    #[cfg(target_os = "linux")]
    if let Some(cgroup) = &cgroup {
        sandbox::attach_cgroup_limits(cgroup, &child);
    }

    let mut stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HookError::SpawnFailed("hook stdout was not piped".to_string()))?;
    // Draining stderr concurrently is not optional: a hook that fills the
    // stderr pipe blocks forever if nobody reads it while stdout is read.
    let stderr_task = tokio::spawn(read_stderr(child.stderr.take()));

    // Deliver the input. See `classify_input_write`: a hook that closes stdin
    // without reading it has declined its input (benign); any other write error
    // means the input genuinely did not arrive (fail closed).
    if let Some(pipe) = stdin.as_mut() {
        let write: std::io::Result<()> = async {
            pipe.write_all(input_json.as_bytes()).await?;
            // In streaming mode the input is the first *line* of a
            // line-delimited protocol, so it needs a terminator before any
            // response can follow. The one-way contract keeps the exact
            // bytes it always had.
            if hook.stream {
                pipe.write_all(b"\n").await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = &write {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                tracing::debug!(
                    hook = hook.label(),
                    event = input.event,
                    "hook closed stdin without reading its input"
                );
            }
        }
        classify_input_write(write)?;
    }

    // The one-way contract ends stdin after the input: a hook that reads
    // stdin to EOF (`sys.stdin.read()`) must see that EOF. Only a hook that
    // declared `stream: true` is kept open for responses and cancel.
    if !hook.stream {
        stdin = None;
    }

    let mut reader = BufReader::new(stdout);
    let mut captured = String::new();
    let mut events = Vec::new();
    let mut total_bytes = 0usize;
    let mut hook_result: Option<HookResult> = None;

    loop {
        // Read incrementally: a hook may ask a question mid-stream and wait
        // for the answer, so buffering the whole body would deadlock.
        let read = tokio::select! {
            read = stream::read_line_capped(&mut reader, MAX_HOOK_LINE_BYTES) => read,
            () = tokio::time::sleep_until(deadline) => {
                cancel_hook(&mut stdin).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(HookError::Timeout(hook.timeout_ms));
            }
        };

        let Some(line) = read.map_err(|error| map_line_error(&error))? else {
            break;
        };

        total_bytes = total_bytes.saturating_add(line.len());
        if total_bytes > MAX_HOOK_OUTPUT_BYTES {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(HookError::OutputTooLarge(MAX_HOOK_OUTPUT_BYTES));
        }

        let line = String::from_utf8_lossy(&line);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Lines after the answer are drained (so the hook is not blocked on a
        // full stdout pipe) but not interpreted: the first answer wins.
        if hook_result.is_some() {
            continue;
        }

        captured.push_str(trimmed);
        captured.push('\n');

        match stream::parse_message(trimmed) {
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(HookError::UnparseableOutput(error));
            }
            Ok(None) => {}
            Ok(Some(HookMessage::Event(event))) => {
                if events.len() < MAX_HOOK_EVENTS {
                    tracing::debug!(
                        hook = hook.label(),
                        event = %event.event,
                        "hook event"
                    );
                    events.push(event);
                }
            }
            Ok(Some(HookMessage::Log(message))) => {
                tracing::info!(hook = hook.label(), "hook log: {message}");
            }
            Ok(Some(HookMessage::Request(request))) => {
                let answer = stream::response_line(&request, request_handler);
                write_answer(&mut stdin, &request, &answer).await?;
            }
            Ok(Some(HookMessage::Result)) => {
                // `{}` and `{"action":"continue"}` are how a hook says
                // "continue"; unparseable output is not a silent continue.
                hook_result = Some(
                    parse_hook_output_line(trimmed)
                        .map_err(|e| HookError::UnparseableOutput(e.to_string()))?,
                );
            }
        }
    }

    // stdout closed, but the process may still be running. Wait for it under
    // the same deadline: a hook that prints its answer and then hangs is a
    // failed hook, exactly as it was when the whole output was buffered.
    let status = if let Ok(status) = tokio::time::timeout_at(deadline, child.wait()).await {
        status.map_err(|e| HookError::SpawnFailed(e.to_string()))?
    } else {
        cancel_hook(&mut stdin).await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(HookError::Timeout(hook.timeout_ms));
    };

    let stderr = stderr_task.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    // A hook that did not exit cleanly did not answer, even if it printed a
    // result first: the exit status is part of the answer.
    if exit_code != 0 {
        return Err(HookError::ExecutionFailed(exit_code));
    }

    let hook_result = hook_result.ok_or(HookError::NoOutput)?;

    Ok(HookExecutionResult {
        hook_result,
        stdout: captured,
        stderr,
        exit_code,
        events,
    })
}

/// Send one answer to a hook that asked a question.
async fn write_answer(
    stdin: &mut Option<ChildStdin>,
    request: &crate::stream::HookHostRequest,
    answer: &str,
) -> Result<(), HookError> {
    let Some(pipe) = stdin.as_mut() else {
        return Err(HookError::InputNotDelivered(format!(
            "the hook asked '{}' but did not declare `stream: true`, so its stdin is closed \
             and the answer cannot be delivered",
            request.method
        )));
    };

    let mut payload = answer.to_string();
    payload.push('\n');
    pipe.write_all(payload.as_bytes()).await.map_err(|error| {
        HookError::InputNotDelivered(format!(
            "the answer to '{}' could not be delivered: {error}",
            request.method
        ))
    })?;
    pipe.flush().await.map_err(|error| {
        HookError::InputNotDelivered(format!(
            "the answer to '{}' could not be flushed: {error}",
            request.method
        ))
    })
}

/// Ask a hook that has run past its deadline to stop, best effort.
async fn cancel_hook(stdin: &mut Option<ChildStdin>) {
    if let Some(pipe) = stdin.as_mut() {
        let _ = pipe.write_all(stream::CANCEL_LINE.as_bytes()).await;
        let _ = pipe.write_all(b"\n").await;
        let _ = pipe.flush().await;
    }
}

/// Read stderr to the end, keeping at most [`MAX_HOOK_STDERR_BYTES`].
///
/// The rest is still drained: refusing to read would fill the pipe and block
/// the hook.
async fn read_stderr(stderr: Option<ChildStderr>) -> String {
    let Some(stderr) = stderr else {
        return String::new();
    };

    let mut reader = BufReader::new(stderr);
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let room = MAX_HOOK_STDERR_BYTES.saturating_sub(kept.len());
                if room > 0 {
                    kept.extend_from_slice(&chunk[..count.min(room)]);
                }
            }
        }
    }

    String::from_utf8_lossy(&kept).into_owned()
}

/// Map a stdout read failure onto the hook's error type.
fn map_line_error(error: &std::io::Error) -> HookError {
    if error.kind() == std::io::ErrorKind::InvalidData {
        HookError::OutputTooLarge(MAX_HOOK_LINE_BYTES)
    } else {
        HookError::SpawnFailed(format!("failed to read hook stdout: {error}"))
    }
}

/// POST a hook's input to its endpoint and read the answer from the body.
///
/// The failure rules are the command hook's rules, one for one: a request that
/// does not complete, a status that is not a success, an empty body, and a body
/// that is not a hook result are all failures, and a failure is what a gating
/// event denies on.
async fn execute_http_hook(
    hook: &HookConfig,
    input: &HookInput,
) -> Result<HookExecutionResult, HookError> {
    let Some(http) = hook.http.as_ref() else {
        return Err(HookError::InvalidHttpHook(
            "no url configured for an http hook".to_string(),
        ));
    };

    if http.block_private_hosts {
        coderex_core::hook_security::SsrfGuard::guard(&http.url)
            .map_err(|e| HookError::InvalidHttpHook(e.to_string()))?;
    }

    let client = reqwest::Client::builder()
        // The body is the conversation. A redirect would hand it to a
        // destination the configuration did not name, so a 3xx is a failure
        // rather than something to follow.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| HookError::RequestFailed(e.to_string()))?;

    let mut request = client.post(&http.url).json(input);
    for (name, value) in &http.headers {
        let value = coderex_core::hook_security::EnvInterpolator::interpolate(value)
            .map_err(|e| HookError::RequestFailed(format!("header '{name}': {e}")))?;
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .map_err(|e| HookError::RequestFailed(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(HookError::HttpStatus(status.as_u16()));
    }

    let body = response
        .text()
        .await
        .map_err(|e| HookError::RequestFailed(e.to_string()))?;
    if body.trim().is_empty() {
        return Err(HookError::NoOutput);
    }
    let hook_result =
        parse_hook_output(&body).map_err(|e| HookError::UnparseableOutput(e.to_string()))?;

    Ok(HookExecutionResult {
        hook_result,
        stdout: body,
        stderr: String::new(),
        exit_code: 0,
        events: Vec::new(),
    })
}

/// Ask a model one question and read the hook's answer from its reply.
///
/// Both `prompt` and `agent` hooks go through here: the difference is the
/// framing ([`ActionPrompt::agent`]), the model the loader defaulted, and the
/// timeout it defaulted — not the protocol.
///
/// Without an installed runner this is a **failure**, not a skip: the gate
/// cannot be answered, and on a gating event that denies.
async fn execute_prompt_hook(
    hook: &HookConfig,
    input: &HookInput,
    action_runner: Option<&dyn HookActionRunner>,
    agent: bool,
) -> Result<HookExecutionResult, HookError> {
    let kind = if agent { "agent" } else { "prompt" };
    let runner = action_runner.ok_or(HookError::MissingActionRunner { kind })?;
    let Some(prompt) = hook.prompt.as_deref() else {
        return Err(HookError::InvalidPromptHook(format!(
            "no 'prompt' configured for this {kind} hook"
        )));
    };

    // The prompt sees what it is gating: the same JSON a command hook gets on
    // stdin, substituted into `$ARGUMENTS`.
    let input_json = serde_json::to_string(input)?;
    let request = ActionPrompt {
        prompt: substitute_arguments(prompt, &input_json),
        model: hook.model.clone(),
        agent,
    };

    let answer = runner.answer(&request).await?;
    let hook_result = parse_answer(&answer)?;

    Ok(HookExecutionResult {
        hook_result,
        stdout: answer,
        stderr: String::new(),
        exit_code: 0,
        events: Vec::new(),
    })
}

/// Call an MCP tool and read the hook's answer from its result.
///
/// The tool input's `${tool_input.<path>}` references are resolved against the
/// hook input first; a reference that does not resolve fails the hook rather
/// than reaching the tool with an empty argument. Without an installed runner
/// this is a failure, exactly as for `prompt` and `agent`.
async fn execute_mcp_tool_hook(
    hook: &HookConfig,
    input: &HookInput,
    action_runner: Option<&dyn HookActionRunner>,
) -> Result<HookExecutionResult, HookError> {
    let runner = action_runner.ok_or(HookError::MissingActionRunner { kind: "mcp_tool" })?;
    let Some(mcp) = hook.mcp.as_ref() else {
        return Err(HookError::InvalidMcpToolHook(
            "no 'server'/'tool' configured for this mcp_tool hook".to_string(),
        ));
    };

    let tool_input = interpolate_tool_input(&mcp.input, input)?;
    let answer = runner
        .call_mcp_tool(&mcp.server, &mcp.tool, &tool_input)
        .await?;
    let hook_result = parse_answer(&answer)?;

    Ok(HookExecutionResult {
        hook_result,
        stdout: answer,
        stderr: String::new(),
        exit_code: 0,
        events: Vec::new(),
    })
}

/// Classify the result of writing a hook's input to its stdin.
///
/// A **broken pipe** is not a delivery failure. It means the hook closed stdin
/// without reading it — the hook declined the input, which a hook that does not
/// look at stdin legitimately does. Treating it as failure would break every
/// such hook, and it is racy: whether the write lands before the hook exits
/// depends on scheduling, so the same hook would pass or fail run to run. A
/// hook that *needed* the input and died instead shows up as a non-zero exit or
/// no output, which fail on their own.
///
/// Any other write error means the input did not arrive, so the hook's answer
/// cannot be trusted: fail closed.
fn classify_input_write(result: std::io::Result<()>) -> Result<(), HookError> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(HookError::InputNotDelivered(e.to_string())),
    }
}

/// Parse a HookResult from a hook's stdout (single JSON line).
pub(crate) fn parse_hook_output(stdout: &str) -> Result<HookResult, HookError> {
    let line = stdout.lines().next().ok_or(HookError::ExecutionFailed(0))?;
    parse_hook_output_line(line)
}

/// Parse a single JSON line into a HookResult.
pub fn parse_hook_output_line(line: &str) -> Result<HookResult, HookError> {
    let parsed: serde_json::Value = serde_json::from_str(line)?;

    let action = parsed
        .get("action")
        .and_then(|v| v.as_str())
        .map_or(HookAction::Continue, |s| match s {
            "stop" => HookAction::Stop,
            _ => HookAction::Continue,
        });

    let suppress_output = parsed
        .get("suppress_output")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let updated_input = parsed
        .get("updated_input")
        .and_then(|v| v.as_str())
        .map(String::from);

    let additional_context = parsed
        .get("additional_context")
        .and_then(|v| v.as_str())
        .map(String::from);

    let watch_paths = parsed
        .get("watch_paths")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let permission_decision = parsed.get("permission_decision").and_then(|v| {
        serde_json::from_value::<crate::events::HookPermissionDecision>(v.clone()).ok()
    });

    Ok(HookResult {
        action,
        suppress_output,
        updated_input,
        additional_context,
        watch_paths,
        permission_decision,
    })
}

/// Build the environment map for a hook subprocess.
fn build_hook_env(hook: &HookConfig, input: &HookInput) -> HashMap<String, String> {
    let mut env = std::env::vars().collect::<HashMap<_, _>>();

    // Inject hook metadata
    env.insert("CODEREX_HOOK_EVENT".to_string(), input.event.clone());
    if let Some(ref session_id) = input.session_id {
        env.insert("CODEREX_SESSION_ID".to_string(), session_id.clone());
    }

    // Inject hook-specific env
    for (key, value) in &hook.env {
        env.insert(key.clone(), value.clone());
    }

    env
}

/// Helper to spawn a hook process (exposed for testing).
#[cfg(test)]
pub fn spawn_hook_process(command: &str, _input: &str) -> std::process::Child {
    use std::process::{Command as StdCommand, Stdio};
    StdCommand::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn test hook process")
}

// ─── Hook Matcher ───────────────────────────────────────────────────────────

/// Matcher for finding hooks by event.
#[derive(Debug)]
pub struct HookMatcher<'a> {
    registry: &'a HookRegistry,
}

impl<'a> HookMatcher<'a> {
    /// Create a new matcher from a registry.
    #[must_use]
    pub const fn new(registry: &'a HookRegistry) -> Self {
        Self { registry }
    }

    /// Find all hooks matching the given event.
    #[must_use]
    pub fn matches(&self, event: &HookEvent) -> Vec<&'a HookConfig> {
        self.registry.matches_for_event(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_registry_starts_empty() {
        let registry = HookRegistry::new();
        assert!(
            registry
                .matches_for_event(&HookEvent::SessionStart)
                .is_empty()
        );
    }

    #[test]
    fn register_and_match_single_hook() {
        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "SessionStart".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo hello".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        });

        let matches = registry.matches_for_event(&HookEvent::SessionStart);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command, "echo hello");
    }

    #[test]
    fn wildcard_hook_matches_all_events() {
        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "*".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo all".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        });

        assert_eq!(
            registry.matches_for_event(&HookEvent::SessionStart).len(),
            1
        );
        assert_eq!(registry.matches_for_event(&HookEvent::Stop).len(), 1);
    }

    /// Settings written with the historical `PostToolUseFail` spelling used to
    /// parse but then never match: matching compared the raw configured string
    /// against the canonical fired name. Both sides are canonicalized now.
    #[tokio::test]
    async fn legacy_event_alias_hook_fires_on_the_canonical_event() {
        let mut registry = HookRegistry::new();
        registry.register(tiered(
            "PostToolUseFail",
            r#"echo '{"action":"continue","additional_context":"alias fired"}'"#,
            HookTier::Core,
            0,
        ));

        let results = registry
            .execute_sync(
                &HookEvent::PostToolUseFailure,
                &HookInput {
                    event: "PostToolUseFailure".to_string(),
                    session_id: None,
                    input: Some("{}".to_string()),
                    context: None,
                },
            )
            .await;

        assert_eq!(results.len(), 1, "the alias must match the fired name");
        assert_eq!(
            results[0]
                .as_ref()
                .unwrap()
                .hook_result
                .additional_context
                .as_deref(),
            Some("alias fired")
        );
    }

    /// Canonicalizing the configured name must not turn unknown names into
    /// matches, and the wildcard must keep matching every event.
    #[test]
    fn canonical_matching_keeps_wildcards_and_drops_unknown_names() {
        let mut registry = HookRegistry::new();
        registry.register(tiered("*", "wildcard", HookTier::Core, 0));
        registry.register(tiered("NoSuchEvent", "unknown", HookTier::Core, 0));

        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command, "wildcard");
    }

    #[test]
    fn load_from_settings_parses_hooks() {
        let value = serde_json::json!({
            "hooks": [
                {
                    "event": "PreToolUse",
                    "command": "echo pre",
                    "env": {"FOO": "bar"}
                }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 1);
    }

    /// `Settings.hooks` is the bare array under the `hooks` key, not a wrapped
    /// object. The install API must accept that shape, or the one-line wiring
    /// does not work.
    #[test]
    fn load_from_settings_accepts_a_bare_hooks_array() {
        let value = serde_json::json!([
            { "event": "PreToolUse", "command": "echo pre" }
        ]);
        let registry = HookRegistry::from_settings_json(&value).unwrap();
        assert_eq!(registry.matches_for_event(&HookEvent::PreToolUse).len(), 1);
    }

    #[test]
    fn load_from_settings_null_returns_empty() {
        let registry = HookRegistry::load_from_settings(&serde_json::Value::Null).unwrap();
        assert!(
            registry
                .matches_for_event(&HookEvent::SessionStart)
                .is_empty()
        );
    }

    #[test]
    fn load_from_settings_invalid_json_errors() {
        let value = serde_json::json!({"hooks": "not an array"});
        let result = HookRegistry::load_from_settings(&value);
        assert!(result.is_err());
    }

    #[test]
    fn parse_hook_output_line_continue() {
        let line = r#"{"action": "continue", "suppress_output": false}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(matches!(result.action, HookAction::Continue));
        assert!(!result.suppress_output);
    }

    #[test]
    fn parse_hook_output_line_stop() {
        let line = r#"{"action": "stop", "suppress_output": true}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(matches!(result.action, HookAction::Stop));
        assert!(result.suppress_output);
    }

    #[test]
    fn parse_hook_output_line_updated_input() {
        let line = r#"{"action": "continue", "updated_input": "new prompt"}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert_eq!(result.updated_input, Some("new prompt".to_string()));
    }

    #[test]
    fn parse_hook_output_line_watch_paths() {
        let line = r#"{"action": "continue", "watch_paths": ["/src", "/lib"]}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert_eq!(result.watch_paths, vec!["/src", "/lib"]);
    }

    #[test]
    fn parse_hook_output_line_permission_decision() {
        let line = r#"{"action": "continue", "permission_decision": {"allow": true, "allow_for_path": null, "reason": "safe"}}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(result.permission_decision.is_some());
        let decision = result.permission_decision.unwrap();
        assert!(decision.allow);
    }

    #[test]
    fn parse_hook_output_line_missing_fields_defaults() {
        let line = r#"{}"#;
        let result = parse_hook_output_line(line).unwrap();
        assert!(matches!(result.action, HookAction::Continue));
        assert!(!result.suppress_output);
        assert!(result.updated_input.is_none());
        assert!(result.watch_paths.is_empty());
    }

    #[test]
    fn parse_hook_output_line_invalid_json() {
        let line = "not json";
        let result = parse_hook_output_line(line);
        assert!(matches!(result, Err(HookError::JsonError(_))));
    }

    #[test]
    fn hook_matcher_matches_registered_hooks() {
        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "PreToolUse".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        });
        registry.register(HookConfig {
            event: "*".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo all".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        });

        let matcher = HookMatcher::new(&registry);
        let matches = matcher.matches(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn hook_matcher_no_match_for_unregistered_event() {
        let registry = HookRegistry::new();
        let matcher = HookMatcher::new(&registry);
        assert!(matcher.matches(&HookEvent::Stop).is_empty());
    }

    #[test]
    fn build_hook_env_includes_metadata() {
        let hook = HookConfig {
            event: "SessionStart".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: [("MY_VAR".to_string(), "my_val".to_string())]
                .into_iter()
                .collect(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        };
        let input = HookInput {
            event: "SessionStart".to_string(),
            session_id: Some("sess_123".to_string()),
            input: None,
            context: None,
        };
        let env = build_hook_env(&hook, &input);
        assert_eq!(env.get("CODEREX_HOOK_EVENT").unwrap(), "SessionStart");
        assert_eq!(env.get("CODEREX_SESSION_ID").unwrap(), "sess_123");
        assert_eq!(env.get("MY_VAR").unwrap(), "my_val");
    }

    #[test]
    fn event_as_str_maps_correctly() {
        assert_eq!(HookEvent::PreToolUse.as_str(), "PreToolUse");
        assert_eq!(HookEvent::PostToolUseFailure.as_str(), "PostToolUseFailure");
        assert_eq!(HookEvent::PostCompact.as_str(), "PostCompact");
        assert_eq!(HookEvent::Stop.as_str(), "Stop");
    }

    #[test]
    fn event_name_round_trips_and_accepts_legacy_alias() {
        use std::str::FromStr;
        for event in <HookEvent as strum::IntoEnumIterator>::iter() {
            assert_eq!(HookEvent::from_str(event.as_str()), Ok(event));
        }
        // 历史写法仍可解析到同一事件
        assert_eq!(
            HookEvent::from_str("PostToolUseFail"),
            Ok(HookEvent::PostToolUseFailure)
        );
        assert!(HookEvent::from_str("NoSuchEvent").is_err());
    }

    #[tokio::test]
    async fn execute_hook_returns_continue_for_answering_command() {
        let hook = HookConfig {
            event: "SessionStart".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: r#"echo '{"action":"continue"}'"#.to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        };
        let input = HookInput {
            event: "SessionStart".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input, None, None)
            .await
            .unwrap();
        assert!(matches!(result.hook_result.action, HookAction::Continue));
        assert_eq!(result.exit_code, 0);
    }

    /// A hook that prints something that is not a hook result has not answered,
    /// so the run is an error rather than a silent Continue.
    #[tokio::test]
    async fn execute_hook_rejects_unparseable_output() {
        let hook = HookConfig {
            event: "SessionStart".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo hello".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        };
        let input = HookInput {
            event: "SessionStart".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input, None, None).await;
        assert!(matches!(result, Err(HookError::UnparseableOutput(_))));
    }

    #[tokio::test]
    async fn execute_hook_timeout_kills_process() {
        let hook = HookConfig {
            event: "Stop".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "sleep 10".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 50,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        };
        let input = HookInput {
            event: "Stop".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input, None, None).await;
        assert!(matches!(result, Err(HookError::Timeout(50))));
    }

    /// The old shell prefix could be undone by the hook itself. The limits are
    /// installed before exec now, so `ulimit -f unlimited` inside the hook
    /// cannot raise them and the write still fails.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_cannot_raise_its_own_limits() {
        let target =
            std::env::temp_dir().join(format!("coderex-hook-limit-engine-{}", std::process::id()));
        let hook = HookConfig {
            event: "Test".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: format!(
                "ulimit -f unlimited 2>/dev/null || true; \
                 head -c 2097152 /dev/zero > {}; echo '{{}}'",
                target.display()
            ),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 10_000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: Some(1),
            sandbox: None,
        };
        let input = HookInput {
            event: "Test".to_string(),
            session_id: None,
            input: None,
            context: None,
        };

        let result = execute_hook_with_timeout(&hook, &input, None, None).await;
        let written = std::fs::metadata(&target).map_or(0, |meta| meta.len());
        let _ = std::fs::remove_file(&target);

        assert!(
            result.is_ok(),
            "the hook's own answer should still be delivered: {result:?}"
        );
        assert!(
            written <= 1024 * 1024,
            "the hook raised its own file-size limit and wrote {written} bytes"
        );
    }

    /// `RLIMIT_CPU` is enforced by the kernel, not by the hook: a busy loop is
    /// stopped after the configured second even though the hook never checks
    /// anything.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_burns_cpu_past_its_limit_is_stopped() {
        let hook = HookConfig {
            event: "Test".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "while :; do :; done".to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 10_000,
            priority: 0,
            dry_run: false,
            stream: false,
            max_cpu_seconds: Some(1),
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
        };
        let input = HookInput {
            event: "Test".to_string(),
            session_id: None,
            input: None,
            context: None,
        };

        let result = execute_hook_with_timeout(&hook, &input, None, None).await;
        assert!(
            matches!(
                result,
                Err(HookError::ExecutionFailed(_) | HookError::Timeout(_))
            ),
            "the CPU limit did not stop the hook: {result:?}"
        );
    }

    // ─── Chain order: managed → plugin → core ───────────────────────────────

    fn tiered(event: &str, command: &str, tier: HookTier, priority: u32) -> HookConfig {
        HookConfig {
            event: event.to_string(),
            kind: HookType::Command,
            command: command.to_string(),
            http: None,
            prompt: None,
            model: None,
            mcp: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority,
            dry_run: false,
            stream: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
            sandbox: None,
            tier,
        }
    }

    /// Tier decides order before priority: a repository-controlled core hook
    /// with the highest priority still runs after a managed hook.
    #[test]
    fn chain_order_is_tier_then_priority() {
        let mut registry = HookRegistry::new();
        registry.register(tiered("PreToolUse", "core", HookTier::Core, 100));
        registry.register(tiered("PreToolUse", "plugin", HookTier::Plugin, 0));
        registry.register(tiered("PreToolUse", "managed", HookTier::Managed, 0));
        registry.register(tiered("PreToolUse", "core-low", HookTier::Core, 1));

        let commands: Vec<&str> = registry
            .matches_for_event(&HookEvent::PreToolUse)
            .iter()
            .map(|h| h.command.as_str())
            .collect();
        assert_eq!(commands, vec!["managed", "plugin", "core", "core-low"]);
    }

    /// Loading separate source documents into one registry keeps the tier of
    /// each source: a managed hook runs before a core hook that gives itself a
    /// higher priority, and the managed document is not also installed as core.
    #[test]
    fn multi_source_loading_preserves_tiers() {
        let managed = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "command": "managed", "priority": 0 }]
        });
        let core = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "command": "core", "priority": 100 }]
        });
        let registry = HookRegistry::from_settings_sources([
            (HookTier::Core, &core),
            (HookTier::Managed, &managed),
        ])
        .unwrap();

        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        let commands: Vec<&str> = matches.iter().map(|h| h.command.as_str()).collect();
        assert_eq!(commands, vec!["managed", "core"]);
        assert_eq!(matches[0].tier, HookTier::Managed);
        assert_eq!(matches[1].tier, HookTier::Core);
        // Exactly two hooks: the managed document was installed once, at its
        // own tier, not folded into core.
        assert_eq!(matches.len(), 2);
    }

    /// A bad entry in the repository-controlled core document must not remove
    /// the administrator's managed `PreToolUse` gate. Before fault isolation a
    /// single typo made `from_settings_sources` return `Err` and the CLI
    /// install no hooks at all.
    #[test]
    fn a_malformed_core_hook_does_not_suppress_a_managed_hook() {
        let managed = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "command": "managed-gate" }]
        });
        // `command` is required for the `command` kind, so this entry is
        // invalid while the document shape itself is fine.
        let core = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "command" }]
        });

        let registry = HookRegistry::from_settings_sources([
            (HookTier::Managed, &managed),
            (HookTier::Core, &core),
        ])
        .unwrap();

        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command, "managed-gate");
        assert_eq!(matches[0].tier, HookTier::Managed);
    }

    /// Entry-level fault isolation applies both when an entry cannot be
    /// deserialized and when it deserializes but cannot be validated: the
    /// invalid entries are skipped and the valid entry in the same source is
    /// kept.
    #[test]
    fn one_invalid_entry_does_not_discard_valid_entries_in_its_source() {
        let value = serde_json::json!({
            "hooks": [
                // No `event`: fails to deserialize.
                { "command": "missing-event" },
                // Deserializes, but a `command` hook with no command is invalid.
                { "event": "PreToolUse", "type": "command" },
                // The one that must survive.
                { "event": "PreToolUse", "command": "kept" }
            ]
        });

        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let commands: Vec<&str> = registry
            .matches_for_event(&HookEvent::PreToolUse)
            .iter()
            .map(|h| h.command.as_str())
            .collect();
        assert_eq!(commands, vec!["kept"]);
    }

    /// A structurally invalid document (here `hooks` is not an array) is
    /// isolated to its own source. The sources around it still load, whichever
    /// tier they are.
    #[test]
    fn a_structurally_invalid_source_does_not_discard_other_sources() {
        let bad = serde_json::json!({ "hooks": "not an array" });
        let managed = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "command": "managed-gate" }]
        });
        let core = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "command": "core-hook" }]
        });

        // A broken managed source must not discard the core source.
        let registry = HookRegistry::from_settings_sources([
            (HookTier::Managed, &bad),
            (HookTier::Core, &core),
        ])
        .unwrap();
        let commands: Vec<&str> = registry
            .matches_for_event(&HookEvent::PreToolUse)
            .iter()
            .map(|h| h.command.as_str())
            .collect();
        assert_eq!(commands, vec!["core-hook"]);

        // And a broken core source must not discard the managed source.
        let registry = HookRegistry::from_settings_sources([
            (HookTier::Managed, &managed),
            (HookTier::Core, &bad),
        ])
        .unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command, "managed-gate");
        assert_eq!(matches[0].tier, HookTier::Managed);
    }

    /// Within a tier, higher priority still wins, and equal keys keep
    /// registration order.
    #[test]
    fn priority_breaks_ties_within_a_tier() {
        let mut registry = HookRegistry::new();
        registry.register(tiered("PreToolUse", "a", HookTier::Core, 0));
        registry.register(tiered("PreToolUse", "b", HookTier::Core, 5));
        registry.register(tiered("PreToolUse", "c", HookTier::Core, 5));

        let commands: Vec<&str> = registry
            .matches_for_event(&HookEvent::PreToolUse)
            .iter()
            .map(|h| h.command.as_str())
            .collect();
        assert_eq!(commands, vec!["b", "c", "a"]);
    }

    /// The tier is a security property: a settings file that writes `tier`
    /// must not be able to promote its hook.
    #[test]
    fn settings_cannot_set_the_tier() {
        let value = serde_json::json!({
            "hooks": [
                { "event": "PreToolUse", "command": "echo hi", "tier": "managed" }
            ]
        });
        let registry = HookRegistry::load_from_settings_with_tier(&value, HookTier::Core).unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].tier, HookTier::Core);
    }

    #[test]
    fn register_in_tier_overrides_the_config_tier() {
        let mut registry = HookRegistry::new();
        registry.register_in_tier(
            HookTier::Managed,
            tiered("PreToolUse", "echo hi", HookTier::Core, 0),
        );
        assert_eq!(
            registry.matches_for_event(&HookEvent::PreToolUse)[0].tier,
            HookTier::Managed
        );
    }

    // ─── Hook kind (`type`) ─────────────────────────────────────────────────

    #[test]
    fn hook_type_defaults_to_command() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "command": "echo hi" }]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        assert_eq!(
            registry.matches_for_event(&HookEvent::PreToolUse)[0].kind,
            HookType::Command
        );
    }

    /// All five kinds load — none is skipped as "recognised but unimplemented".
    #[test]
    fn every_hook_kind_loads() {
        let value = serde_json::json!({
            "hooks": [
                { "event": "PreToolUse", "type": "prompt", "prompt": "is this ok?" },
                { "event": "PreToolUse", "type": "mcp_tool", "server": "s", "tool": "t" },
                { "event": "PreToolUse", "type": "agent", "prompt": "verify" },
                { "event": "PreToolUse", "type": "http", "url": "https://example.test/hook" },
                { "event": "PreToolUse", "command": "echo hi" }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let kinds: Vec<HookType> = registry
            .matches_for_event(&HookEvent::PreToolUse)
            .iter()
            .map(|h| h.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                HookType::Prompt,
                HookType::McpTool,
                HookType::Agent,
                HookType::Http,
                HookType::Command,
            ]
        );
    }

    /// A `prompt` hook with no `prompt` — and an `agent` one — is an invalid
    /// entry: it is skipped, and a valid entry beside it still loads. The field
    /// is what the kind *is*, so the entry cannot be repaired.
    #[test]
    fn a_prompt_or_agent_hook_without_a_prompt_is_skipped() {
        for kind in ["prompt", "agent"] {
            let value = serde_json::json!({
                "hooks": [
                    { "event": "PreToolUse", "type": kind },
                    { "event": "PreToolUse", "command": "kept" }
                ]
            });
            let registry = HookRegistry::load_from_settings(&value).unwrap();
            let matches = registry.matches_for_event(&HookEvent::PreToolUse);
            assert_eq!(
                matches.len(),
                1,
                "a {kind} hook with no prompt must be skipped, not fail the source"
            );
            assert_eq!(matches[0].command, "kept");
        }
    }

    /// An `mcp_tool` hook must name both a server and a tool. One missing
    /// either is skipped; a valid entry in the same document still loads.
    #[test]
    fn an_mcp_tool_hook_without_a_server_or_tool_is_skipped() {
        for hook in [
            serde_json::json!({ "event": "PreToolUse", "type": "mcp_tool", "tool": "t" }),
            serde_json::json!({ "event": "PreToolUse", "type": "mcp_tool", "server": "s" }),
        ] {
            let value = serde_json::json!({
                "hooks": [
                    hook,
                    { "event": "PreToolUse", "command": "kept" }
                ]
            });
            let registry = HookRegistry::load_from_settings(&value).unwrap();
            let matches = registry.matches_for_event(&HookEvent::PreToolUse);
            assert_eq!(
                matches.len(),
                1,
                "an mcp_tool hook must name both server and tool; the bad entry is skipped"
            );
            assert_eq!(matches[0].command, "kept");
        }
    }

    /// The two model-backed kinds differ on purpose: an `agent` hook is a
    /// verifier with a cheap model and a longer timeout, a `prompt` hook is one
    /// question on the session's model.
    #[test]
    fn agent_and_prompt_hook_defaults_differ() {
        let value = serde_json::json!({
            "hooks": [
                { "event": "PreToolUse", "type": "prompt", "prompt": "ask" },
                { "event": "PreToolUse", "type": "agent", "prompt": "verify" }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        let prompt = matches.iter().find(|h| h.kind == HookType::Prompt).unwrap();
        let agent = matches.iter().find(|h| h.kind == HookType::Agent).unwrap();

        assert_eq!(prompt.timeout_ms, crate::action::PROMPT_DEFAULT_TIMEOUT_MS);
        assert_eq!(agent.timeout_ms, crate::action::AGENT_DEFAULT_TIMEOUT_MS);
        assert_ne!(prompt.timeout_ms, agent.timeout_ms);
        assert_eq!(prompt.model, None, "a prompt hook keeps the session model");
        assert_eq!(
            agent.model.as_deref(),
            Some(crate::action::AGENT_DEFAULT_MODEL)
        );
    }

    /// An explicit model and timeout still win over the kind's defaults.
    #[test]
    fn an_action_hook_keeps_an_explicit_model_and_timeout() {
        let value = serde_json::json!({
            "hooks": [{
                "event": "PreToolUse", "type": "agent", "prompt": "verify",
                "model": "some-model", "timeout_ms": 1234
            }]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let hook = &registry.matches_for_event(&HookEvent::PreToolUse)[0];
        assert_eq!(hook.model.as_deref(), Some("some-model"));
        assert_eq!(hook.timeout_ms, 1234);
    }

    /// `if`, `once`, `statusMessage`, and `continueOnBlock` are accepted — a
    /// settings file written for the reference still loads — and do nothing.
    #[test]
    fn unimplemented_hook_fields_are_accepted_and_inert() {
        let value = serde_json::json!({
            "hooks": [{
                "event": "PreToolUse", "type": "prompt", "prompt": "ask",
                "if": "Bash(git*)", "once": true, "statusMessage": "checking",
                "continueOnBlock": true
            }]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        assert_eq!(registry.matches_for_event(&HookEvent::PreToolUse).len(), 1);
    }

    // ─── Action kinds: prompt, agent, mcp_tool ──────────────────────────────

    fn prompt_config(kind: HookType, prompt: &str) -> HookConfig {
        HookConfig {
            event: "PreToolUse".to_string(),
            kind,
            prompt: Some(prompt.to_string()),
            ..tiered("PreToolUse", "", HookTier::Core, 0)
        }
    }

    fn mcp_config(input: serde_json::Value) -> HookConfig {
        HookConfig {
            event: "PreToolUse".to_string(),
            kind: HookType::McpTool,
            mcp: Some(crate::action::McpToolHook {
                server: "files".to_string(),
                tool: "stat".to_string(),
                input,
            }),
            ..tiered("PreToolUse", "", HookTier::Core, 0)
        }
    }

    fn action_input() -> HookInput {
        HookInput {
            event: "PreToolUse".to_string(),
            session_id: Some("sess_1".to_string()),
            input: Some(r#"{"file_path":"/etc/hosts"}"#.to_string()),
            context: None,
        }
    }

    /// Coerce a concrete test runner to what the dispatcher takes.
    ///
    /// Always `Some` by construction; the `Option` is the dispatcher's
    /// parameter type, and the tests that omit a runner pass `None` directly.
    #[allow(clippy::unnecessary_wraps)]
    fn as_runner(runner: &Arc<crate::action::FakeRunner>) -> Option<&dyn HookActionRunner> {
        Some(runner.as_ref() as &dyn HookActionRunner)
    }

    /// A `prompt` hook dispatches to the runner and its text answer is parsed
    /// with the same parser every other kind uses.
    #[tokio::test]
    async fn a_prompt_hook_dispatches_to_the_runner() {
        let runner = crate::action::FakeRunner::answering(
            r#"{"action":"continue","additional_context":"looks fine"}"#,
        );
        let hook = prompt_config(HookType::Prompt, "is this ok?");
        let result = execute_hook_with_timeout(&hook, &action_input(), as_runner(&runner), None)
            .await
            .unwrap();

        assert!(matches!(result.hook_result.action, HookAction::Continue));
        assert_eq!(
            result.hook_result.additional_context.as_deref(),
            Some("looks fine")
        );
        let prompts = runner.prompts();
        assert_eq!(prompts.len(), 1);
        assert!(!prompts[0].agent, "a prompt hook is not agent framing");
        assert_eq!(
            prompts[0].model, None,
            "a prompt hook uses the session model"
        );
    }

    /// An `agent` hook goes through the same call with the verifier framing.
    #[tokio::test]
    async fn an_agent_hook_dispatches_with_the_agent_framing() {
        let runner = crate::action::FakeRunner::answering(r#"{"action":"stop"}"#);
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "agent", "prompt": "verify $ARGUMENTS" }]
        });
        let hook = HookRegistry::load_from_settings(&value)
            .unwrap()
            .matches_for_event(&HookEvent::PreToolUse)[0]
            .clone();
        let result = execute_hook_with_timeout(&hook, &action_input(), as_runner(&runner), None)
            .await
            .unwrap();

        assert!(matches!(result.hook_result.action, HookAction::Stop));
        let prompts = runner.prompts();
        assert!(prompts[0].agent, "an agent hook must say it is one");
        assert_eq!(
            prompts[0].model.as_deref(),
            Some(crate::action::AGENT_DEFAULT_MODEL)
        );
        // The prompt sees the hook input JSON, with the `input` field escaped
        // the way `serde_json` writes a string inside a string.
        assert!(
            prompts[0]
                .prompt
                .starts_with(r#"verify {"event":"PreToolUse""#),
            "{}",
            prompts[0].prompt
        );
        assert!(
            prompts[0]
                .prompt
                .contains(r#"\"file_path\":\"/etc/hosts\""#)
        );
    }

    /// `$ARGUMENTS` is how a prompt hook sees what it is gating: the hook input
    /// JSON.
    #[tokio::test]
    async fn a_prompt_hook_sees_the_hook_input_as_arguments() {
        let runner = crate::action::FakeRunner::answering("{}");
        let hook = prompt_config(HookType::Prompt, "gate this: $ARGUMENTS");
        execute_hook_with_timeout(&hook, &action_input(), as_runner(&runner), None)
            .await
            .unwrap();

        let prompt = runner.prompts()[0].prompt.clone();
        assert!(prompt.starts_with("gate this: {"), "{prompt}");
        assert!(prompt.contains(r#""event":"PreToolUse""#), "{prompt}");
        assert!(
            prompt.contains(r#""input":"{\"file_path\":\"/etc/hosts\"}""#),
            "{prompt}"
        );
    }

    /// An `mcp_tool` hook calls the tool on the named server, with its
    /// `${tool_input.*}` references resolved against the hook input.
    #[tokio::test]
    async fn an_mcp_tool_hook_calls_the_runner_with_resolved_input() {
        let runner = crate::action::FakeRunner::answering(r#"{"action":"continue"}"#);
        let hook = mcp_config(serde_json::json!({
            "path": "${tool_input.file_path}",
            "recursive": false
        }));
        let result = execute_hook_with_timeout(&hook, &action_input(), as_runner(&runner), None)
            .await
            .unwrap();

        assert!(matches!(result.hook_result.action, HookAction::Continue));
        let calls = runner.mcp_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "files");
        assert_eq!(calls[0].1, "stat");
        assert_eq!(
            calls[0].2,
            serde_json::json!({"path": "/etc/hosts", "recursive": false})
        );
    }

    /// A reference that does not resolve fails the hook rather than calling the
    /// tool with an empty argument.
    #[tokio::test]
    async fn an_unresolvable_tool_input_reference_fails_the_hook() {
        let runner = crate::action::FakeRunner::answering("{}");
        let hook = mcp_config(serde_json::json!({ "path": "${tool_input.nope}" }));
        let result =
            execute_hook_with_timeout(&hook, &action_input(), as_runner(&runner), None).await;

        assert!(
            matches!(result, Err(HookError::InterpolationFailed(_))),
            "{result:?}"
        );
        assert!(
            runner.mcp_calls().is_empty(),
            "the tool must not be called with a missing argument"
        );
    }

    /// A kind that needs a runner, with none installed, is a failure — not a
    /// silent continue. On a gating event that failure denies.
    #[tokio::test]
    async fn an_action_hook_without_a_runner_fails_and_denies_on_a_gate() {
        for hook in [
            prompt_config(HookType::Prompt, "ask"),
            prompt_config(HookType::Agent, "verify"),
            mcp_config(serde_json::json!({})),
        ] {
            let kind = hook.kind;
            let mut registry = HookRegistry::new();
            registry.register(hook);

            let result = registry
                .fire_hook("PreToolUse", "Bash", &HashMap::new())
                .await;

            assert!(result.failed, "{kind:?} without a runner is a failure");
            assert!(result.stop, "{kind:?} failure must deny on a gating event");
            assert!(
                result
                    .errors
                    .iter()
                    .any(|e| e.contains("this session cannot run")),
                "the reason must say what is missing: {:?}",
                result.errors
            );
        }
    }

    /// The same failure off a gating event is recorded, not turned into a stop
    /// — there is nowhere to deny.
    #[tokio::test]
    async fn an_action_hook_without_a_runner_is_recorded_off_a_gate() {
        let mut registry = HookRegistry::new();
        registry.register(HookConfig {
            event: "PostToolUse".to_string(),
            ..prompt_config(HookType::Prompt, "ask")
        });

        let result = registry
            .fire_hook("PostToolUse", "Bash", &HashMap::new())
            .await;
        assert!(result.failed);
        assert!(!result.stop);
    }

    /// With a runner installed, a `prompt` hook's `stop` answer stops a gating
    /// event — the failure above is about the missing runner, not about the
    /// kind being inert.
    #[tokio::test]
    async fn a_prompt_hook_with_a_runner_can_stop_a_gating_event() {
        let mut registry = HookRegistry::new();
        registry.register(prompt_config(HookType::Prompt, "ask"));
        registry.set_action_runner(crate::action::FakeRunner::answering(r#"{"action":"stop"}"#));

        let result = registry
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;
        assert!(result.stop);
        assert!(!result.failed);
    }

    /// An answer that is empty or not a hook result is a failure, exactly as
    /// for a `command` hook's stdout.
    #[tokio::test]
    async fn an_action_hook_that_gives_no_answer_is_a_failure() {
        for answer in ["", "the tool looks fine"] {
            let runner = crate::action::FakeRunner::answering(answer);
            let hook = prompt_config(HookType::Prompt, "ask");
            let result =
                execute_hook_with_timeout(&hook, &action_input(), as_runner(&runner), None).await;
            assert!(result.is_err(), "{answer:?} should not read as an answer");
        }
    }

    /// A runner installed on the registry survives `into_executor`, which is
    /// how the CLI installs one.
    #[tokio::test]
    async fn a_runner_survives_into_executor() {
        let mut registry = HookRegistry::new();
        registry.register(prompt_config(HookType::Prompt, "ask"));
        registry.set_action_runner(crate::action::FakeRunner::answering("{}"));
        assert!(registry.action_runner().is_some());

        let executor = registry.into_executor();
        let result = executor
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;
        assert!(!result.failed, "{:?}", result.errors);
    }

    /// An `http` hook loads, and names itself by its URL.
    #[test]
    fn an_http_hook_loads_and_labels_itself_by_url() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "http", "url": "https://example.test/hook" }]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].kind, HookType::Http);
        assert_eq!(matches[0].label(), "https://example.test/hook");
    }

    #[test]
    fn http_hook_without_a_url_is_skipped() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "http" }]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        assert!(
            registry
                .matches_for_event(&HookEvent::PreToolUse)
                .is_empty()
        );
    }

    /// The body is the conversation, so the destination has to be absolute and
    /// speak http(s) — not a relative path, and not a scheme that reads a file.
    /// An endpoint that fails validation is skipped, not loaded.
    #[test]
    fn http_hook_url_must_be_absolute_http() {
        for url in [
            "/hook",
            "example.test/hook",
            "file:///etc/hosts",
            "ftp://example.test/hook",
        ] {
            let value = serde_json::json!({
                "hooks": [
                    { "event": "PreToolUse", "type": "http", "url": url },
                    { "event": "PreToolUse", "command": "kept" }
                ]
            });
            let registry = HookRegistry::load_from_settings(&value).unwrap();
            let matches = registry.matches_for_event(&HookEvent::PreToolUse);
            assert_eq!(
                matches.len(),
                1,
                "{url} should not be accepted as a hook endpoint"
            );
            assert_eq!(matches[0].command, "kept");
        }
    }

    /// A header may only read a variable the configuration granted it. The
    /// refused entry is skipped while the rest of the document loads.
    #[test]
    fn http_hook_header_cannot_reference_an_ungranted_variable() {
        let refused = serde_json::json!({
            "hooks": [{
                "event": "PreToolUse",
                "type": "http",
                "url": "https://example.test/hook",
                "headers": { "authorization": "Bearer $ANTHROPIC_API_KEY" }
            }, {
                "event": "PreToolUse",
                "command": "kept"
            }]
        });
        let registry = HookRegistry::load_from_settings(&refused).unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(
            matches.len(),
            1,
            "a header read a variable that allowedEnvVars did not grant"
        );
        assert_eq!(matches[0].command, "kept");

        let granted = serde_json::json!({
            "hooks": [{
                "event": "PreToolUse",
                "type": "http",
                "url": "https://example.test/hook",
                "headers": { "authorization": "Bearer $HOOK_TOKEN" },
                "allowedEnvVars": ["HOOK_TOKEN"]
            }]
        });
        assert_eq!(
            HookRegistry::load_from_settings(&granted)
                .unwrap()
                .matches_for_event(&HookEvent::PreToolUse)
                .len(),
            1
        );
    }

    /// An unknown `type` cannot be deserialized; that entry is skipped rather
    /// than failing the source.
    #[test]
    fn unknown_hook_type_is_skipped() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "telepathy", "command": "echo hi" }]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        assert!(
            registry
                .matches_for_event(&HookEvent::PreToolUse)
                .is_empty()
        );
    }

    #[test]
    fn command_type_without_a_command_is_skipped() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "command" }]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        assert!(
            registry
                .matches_for_event(&HookEvent::PreToolUse)
                .is_empty()
        );
    }

    // ─── Fail-closed contract ───────────────────────────────────────────────

    /// A hook that cannot answer must not read as an allow. On a gating event
    /// the failure is surfaced as `stop` so the existing call sites deny.
    #[tokio::test]
    async fn failed_hook_denies_on_a_gating_event() {
        let mut registry = HookRegistry::new();
        registry.register(tiered("PreToolUse", "exit 1", HookTier::Core, 0));

        let result = registry
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;
        assert!(result.failed, "a broken hook is a failure");
        assert!(result.stop, "a failure on a gating event must deny");
        assert!(!result.errors.is_empty());
    }

    /// On an event whose firing site does not read `stop`, a failure can only
    /// be recorded — there is nowhere to deny.
    #[tokio::test]
    async fn failed_hook_is_recorded_but_cannot_deny_off_gate() {
        let mut registry = HookRegistry::new();
        registry.register(tiered("PostToolUse", "exit 1", HookTier::Core, 0));

        let result = registry
            .fire_hook("PostToolUse", "Bash", &HashMap::new())
            .await;
        assert!(result.failed);
        assert!(!result.stop);
        assert!(!result.errors.is_empty());
    }

    /// An answering hook is not a failure, even when it decides to stop.
    #[tokio::test]
    async fn answering_stop_hook_is_not_a_failure() {
        let mut registry = HookRegistry::new();
        registry.register(tiered(
            "PreToolUse",
            r#"echo '{"action":"stop"}'"#,
            HookTier::Core,
            0,
        ));

        let result = registry
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;
        assert!(result.stop);
        assert!(!result.failed);
    }

    /// A hook that closes stdin without reading it declined its input — that is
    /// not a delivery failure, and must not fail the hook. Any other write
    /// error is a delivery failure and must.
    #[test]
    fn input_write_classification() {
        use std::io::{Error, ErrorKind};

        assert!(classify_input_write(Ok(())).is_ok());
        assert!(
            classify_input_write(Err(Error::from(ErrorKind::BrokenPipe))).is_ok(),
            "a hook that ignores stdin must not be treated as failed"
        );
        assert!(matches!(
            classify_input_write(Err(Error::from(ErrorKind::PermissionDenied))),
            Err(HookError::InputNotDelivered(_))
        ));
    }

    /// `into_executor` is the one-line install path the CLI needs.
    #[tokio::test]
    async fn into_executor_installs_a_working_registry() {
        let mut registry = HookRegistry::new();
        registry.register(tiered(
            "PreToolUse",
            r#"echo '{"action":"stop"}'"#,
            HookTier::Core,
            0,
        ));
        let executor = registry.into_executor();
        let result = executor
            .fire_hook("PreToolUse", "Bash", &HashMap::new())
            .await;
        assert!(result.stop);
        assert!(!result.failed);
    }
}
