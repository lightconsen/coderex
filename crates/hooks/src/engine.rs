//! Hook execution engine — configuration, matching, and subprocess execution.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::events::{HookAction, HookError, HookEvent, HookResult};
use coderex_core::hook_executor::{HookExecutor, HookFireResult};

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
/// CodeRex implements [`HookType::Command`] (a subprocess). The other kinds are
/// recognised so that a configuration naming one is reported rather than
/// silently treated as a command; see [`HookRegistry::load_from_settings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    /// Run a shell command (the only kind CodeRex executes).
    #[default]
    Command,
    /// Call an HTTP endpoint. Recognised but not implemented.
    Http,
    /// Ask a model. Recognised but not implemented.
    Prompt,
    /// Call an MCP tool. Recognised but not implemented.
    McpTool,
    /// Run an agent. Recognised but not implemented.
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
    /// Maximum CPU seconds for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    /// Maximum memory (MB) for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Maximum output file size (MB) for the hook subprocess (None = unlimited).
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
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
    /// A hook names itself by its command or, for an `http` hook, by the URL it
    /// posts to — `command` is empty for the latter, and a log line reading
    /// `hook_command=""` says nothing about which hook ran.
    #[must_use]
    pub fn label(&self) -> &str {
        match self.http.as_ref() {
            Some(http) => &http.url,
            None => &self.command,
        }
    }
}

/// Build a shell command wrapper that applies resource limits via `ulimit`.
///
/// Returns `None` if no limits are configured.
fn sandbox_prefix(config: &HookConfig) -> Option<String> {
    let mut parts = Vec::new();

    // RLIMIT_CPU: max CPU time in seconds (-t)
    if let Some(cpu_secs) = config.max_cpu_seconds {
        parts.push(format!("ulimit -t {cpu_secs}"));
    }

    // RLIMIT_FSIZE: max file size in KB (-f), convert MB → KB
    if let Some(size_mb) = config.max_file_size_mb {
        let size_kb = size_mb * 1024;
        parts.push(format!("ulimit -f {size_kb}"));
    }

    // RLIMIT_AS (virtual memory): max in KB (-v), convert MB → KB
    if let Some(mem_mb) = config.max_memory_mb {
        let mem_kb = mem_mb * 1024;
        parts.push(format!("ulimit -v {mem_kb}"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
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

/// The two shapes the hook settings arrive in.
///
/// `Settings.hooks` in `coderex-config` holds the bare array — the value under
/// the `hooks` key of `settings.json` — while a whole settings object is
/// `{"hooks": [...]}`. The install API accepts either so the caller does not
/// have to know which it has.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum HookSettingsValue {
    /// `{"hooks": [...]}` — a whole settings object.
    Wrapped(HookSettings),
    /// `[...]` — the bare array, as `Settings.hooks` holds it.
    Bare(Vec<RawHookConfig>),
}

impl HookSettingsValue {
    /// The hook list, whichever shape it came in.
    fn into_hooks(self) -> Vec<RawHookConfig> {
        match self {
            Self::Wrapped(settings) => settings.hooks,
            Self::Bare(hooks) => hooks,
        }
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
    /// Max CPU seconds.
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    /// Max memory MB.
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Max file size MB.
    #[serde(default)]
    pub max_file_size_mb: Option<u64>,
}

// ─── Hook Execution Result ──────────────────────────────────────────────────

/// Result from executing a hook, including subprocess output.
#[derive(Debug, Clone)]
pub struct HookExecutionResult {
    /// Parsed hook result from stdout.
    pub hook_result: HookResult,
    /// Standard output from the hook subprocess.
    pub stdout: String,
    /// Standard error from the hook subprocess.
    pub stderr: String,
    /// Exit code from the hook subprocess.
    pub exit_code: i32,
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
#[derive(Debug)]
pub struct HookRegistry {
    hooks: Vec<HookConfig>,
}

impl HookRegistry {
    /// Create an empty hook registry.
    #[must_use]
    pub const fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Load hooks from a settings.json value, assigning every hook the given
    /// trust tier.
    ///
    /// A hook whose `type` is a kind CodeRex does not execute is **skipped with
    /// a loud warning**, not silently run as a command and not silently
    /// dropped: only `command` hooks run. A `command` hook with no `command`
    /// field is a configuration error and fails the load.
    pub fn load_from_settings_with_tier(
        value: &serde_json::Value,
        tier: HookTier,
    ) -> Result<Self, HookError> {
        if value.is_null() {
            return Ok(Self::new());
        }
        // Accept both `{"hooks": [...]}` and the bare `[...]` that
        // `Settings.hooks` holds.
        let raw_hooks = serde_json::from_value::<HookSettingsValue>(value.clone())?.into_hooks();
        let mut hooks = Vec::with_capacity(raw_hooks.len());
        for raw in raw_hooks {
            match raw.kind {
                HookType::Command => {
                    let Some(command) = raw.command else {
                        return Err(HookError::InvalidCommand(format!(
                            "hook for event '{}' is type 'command' but has no 'command' field",
                            raw.event
                        )));
                    };
                    hooks.push(HookConfig {
                        event: raw.event,
                        kind: HookType::Command,
                        command,
                        http: None,
                        env: raw.env.unwrap_or_default(),
                        cwd: raw.cwd,
                        internal: raw.internal.unwrap_or(false),
                        timeout_ms: raw.timeout_ms.unwrap_or_else(default_timeout),
                        priority: raw.priority.unwrap_or(0),
                        dry_run: raw.dry_run.unwrap_or(false),
                        max_cpu_seconds: raw.max_cpu_seconds,
                        max_memory_mb: raw.max_memory_mb,
                        max_file_size_mb: raw.max_file_size_mb,
                        tier,
                    });
                }
                HookType::Http => {
                    let Some(url) = raw.url else {
                        return Err(HookError::InvalidHttpHook(format!(
                            "hook for event '{}' is type 'http' but has no 'url' field",
                            raw.event
                        )));
                    };
                    let http = HttpHook {
                        url,
                        headers: raw.headers.unwrap_or_default(),
                        allowed_env_vars: raw.allowed_env_vars.unwrap_or_default(),
                        block_private_hosts: raw.block_private_hosts.unwrap_or(false),
                    };
                    http.validate()?;
                    hooks.push(HookConfig {
                        event: raw.event,
                        kind: HookType::Http,
                        command: String::new(),
                        http: Some(http),
                        env: raw.env.unwrap_or_default(),
                        cwd: raw.cwd,
                        internal: raw.internal.unwrap_or(false),
                        timeout_ms: raw.timeout_ms.unwrap_or_else(default_timeout),
                        priority: raw.priority.unwrap_or(0),
                        dry_run: raw.dry_run.unwrap_or(false),
                        max_cpu_seconds: raw.max_cpu_seconds,
                        max_memory_mb: raw.max_memory_mb,
                        max_file_size_mb: raw.max_file_size_mb,
                        tier,
                    });
                }
                unsupported => tracing::warn!(
                    hook_type = unsupported.as_str(),
                    event = %raw.event,
                    "hook configured with a type CodeRex does not execute — skipping it; \
                     'command' and 'http' hooks run"
                ),
            }
        }
        Self::warn_about_inert_hooks(&hooks);
        Ok(Self { hooks })
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
    fn matches_for_event_name(&self, event_name: &str) -> Vec<&HookConfig> {
        let mut hooks: Vec<_> = self
            .hooks
            .iter()
            .filter(|h| h.event == event_name || h.event == "*")
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
            let result = execute_hook_with_timeout(hook, input).await;
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
            let result = execute_hook_with_timeout(hook, input).await;
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
            let handle =
                tokio::spawn(async move { execute_hook_with_timeout(&hook, &input).await });
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
        for exec_result in results.into_iter() {
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
    let execution = async {
        match hook.kind {
            HookType::Command => execute_hook(hook, input).await,
            HookType::Http => execute_http_hook(hook, input).await,
            // The loader refuses these, so this is only reachable for a
            // `HookConfig` built by hand.
            other => Err(HookError::InvalidCommand(format!(
                "hook type '{}' is not executable",
                other.as_str()
            ))),
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), execution)
        .await
        .map_or(Err(HookError::Timeout(timeout_ms)), |r| r);

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
) -> Result<HookExecutionResult, HookError> {
    let input_json = serde_json::to_string(input)?;
    let env = build_hook_env(hook, input);

    // Wrap command with sandbox limits if configured.
    let shell_cmd = match sandbox_prefix(hook) {
        Some(prefix) => format!("{prefix}; {command}", command = hook.command),
        None => hook.command.clone(),
    };

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&shell_cmd)
        .envs(&env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| HookError::SpawnFailed(e.to_string()))?;

    // Deliver the input. See `classify_input_write`: a hook that closes stdin
    // without reading it has declined its input (benign); any other write error
    // means the input genuinely did not arrive (fail closed).
    if let Some(mut stdin) = child.stdin.take() {
        let write = stdin.write_all(input_json.as_bytes()).await;
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

    // Collect stdout and stderr
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| HookError::SpawnFailed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    // A hook that did not exit cleanly did not answer.
    if exit_code != 0 {
        return Err(HookError::ExecutionFailed(exit_code));
    }
    // Neither did one that printed nothing.
    if stdout.trim().is_empty() {
        return Err(HookError::NoOutput);
    }
    // Nor one whose output is not a hook result. `{}` and
    // `{"action":"continue"}` are how a hook says "continue"; unparseable
    // output is not a silent continue.
    let hook_result =
        parse_hook_output(&stdout).map_err(|e| HookError::UnparseableOutput(e.to_string()))?;

    Ok(HookExecutionResult {
        hook_result,
        stdout,
        stderr,
        exit_code,
    })
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
fn parse_hook_output(stdout: &str) -> Result<HookResult, HookError> {
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
        assert!(registry
            .matches_for_event(&HookEvent::SessionStart)
            .is_empty());
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
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
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
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        });

        assert_eq!(
            registry.matches_for_event(&HookEvent::SessionStart).len(),
            1
        );
        assert_eq!(registry.matches_for_event(&HookEvent::Stop).len(), 1);
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
        assert!(registry
            .matches_for_event(&HookEvent::SessionStart)
            .is_empty());
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
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        });
        registry.register(HookConfig {
            event: "*".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo all".to_string(),
            http: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
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
            env: [("MY_VAR".to_string(), "my_val".to_string())]
                .into_iter()
                .collect(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
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
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        let input = HookInput {
            event: "SessionStart".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input).await.unwrap();
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
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        let input = HookInput {
            event: "SessionStart".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input).await;
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
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 50,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        let input = HookInput {
            event: "Stop".to_string(),
            session_id: None,
            input: None,
            context: None,
        };
        let result = execute_hook_with_timeout(&hook, &input).await;
        assert!(matches!(result, Err(HookError::Timeout(50))));
    }

    #[test]
    fn sandbox_prefix_none_when_no_limits() {
        let hook = HookConfig {
            event: "Test".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo test".to_string(),
            http: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
        };
        assert!(sandbox_prefix(&hook).is_none());
    }

    #[test]
    fn sandbox_prefix_with_limits() {
        let hook = HookConfig {
            event: "Test".to_string(),
            kind: HookType::Command,
            tier: HookTier::Core,
            command: "echo test".to_string(),
            http: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority: 0,
            dry_run: false,
            max_cpu_seconds: Some(10),
            max_memory_mb: Some(256),
            max_file_size_mb: Some(50),
        };
        let prefix = sandbox_prefix(&hook).unwrap();
        assert!(prefix.contains("ulimit -t 10"));
        assert!(prefix.contains("ulimit -v 262144")); // 256 * 1024
        assert!(prefix.contains("ulimit -f 51200")); // 50 * 1024
    }

    // ─── Chain order: managed → plugin → core ───────────────────────────────

    fn tiered(event: &str, command: &str, tier: HookTier, priority: u32) -> HookConfig {
        HookConfig {
            event: event.to_string(),
            kind: HookType::Command,
            command: command.to_string(),
            http: None,
            env: HashMap::new(),
            cwd: None,
            internal: false,
            timeout_ms: 5000,
            priority,
            dry_run: false,
            max_cpu_seconds: None,
            max_memory_mb: None,
            max_file_size_mb: None,
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

    /// A kind CodeRex does not execute is skipped, not run as a command and not
    /// silently accepted — the other hooks in the same settings still load.
    #[test]
    fn unsupported_hook_type_is_skipped_not_run() {
        let value = serde_json::json!({
            "hooks": [
                { "event": "PreToolUse", "type": "prompt", "prompt": "is this ok?" },
                { "event": "PreToolUse", "type": "mcp_tool", "server": "s", "tool": "t" },
                { "event": "PreToolUse", "type": "agent", "prompt": "verify" },
                { "event": "PreToolUse", "command": "echo hi" }
            ]
        });
        let registry = HookRegistry::load_from_settings(&value).unwrap();
        let matches = registry.matches_for_event(&HookEvent::PreToolUse);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].command, "echo hi");
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
    fn http_hook_without_a_url_is_a_config_error() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "http" }]
        });
        assert!(HookRegistry::load_from_settings(&value).is_err());
    }

    /// The body is the conversation, so the destination has to be absolute and
    /// speak http(s) — not a relative path, and not a scheme that reads a file.
    #[test]
    fn http_hook_url_must_be_absolute_http() {
        for url in [
            "/hook",
            "example.test/hook",
            "file:///etc/hosts",
            "ftp://example.test/hook",
        ] {
            let value = serde_json::json!({
                "hooks": [{ "event": "PreToolUse", "type": "http", "url": url }]
            });
            assert!(
                HookRegistry::load_from_settings(&value).is_err(),
                "{url} should not be accepted as a hook endpoint"
            );
        }
    }

    /// A header may only read a variable the configuration granted it.
    #[test]
    fn http_hook_header_cannot_reference_an_ungranted_variable() {
        let refused = serde_json::json!({
            "hooks": [{
                "event": "PreToolUse",
                "type": "http",
                "url": "https://example.test/hook",
                "headers": { "authorization": "Bearer $ANTHROPIC_API_KEY" }
            }]
        });
        assert!(
            HookRegistry::load_from_settings(&refused).is_err(),
            "a header read a variable that allowedEnvVars did not grant"
        );

        let granted = serde_json::json!({
            "hooks": [{
                "event": "PreToolUse",
                "type": "http",
                "url": "https://example.test/hook",
                "headers": { "authorization": "Bearer $HOOK_TOKEN" },
                "allowedEnvVars": ["HOOK_TOKEN"]
            }]
        });
        assert!(HookRegistry::load_from_settings(&granted).is_ok());
    }

    #[test]
    fn unknown_hook_type_is_a_config_error() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "telepathy", "command": "echo hi" }]
        });
        assert!(HookRegistry::load_from_settings(&value).is_err());
    }

    #[test]
    fn command_type_without_a_command_is_a_config_error() {
        let value = serde_json::json!({
            "hooks": [{ "event": "PreToolUse", "type": "command" }]
        });
        assert!(matches!(
            HookRegistry::load_from_settings(&value),
            Err(HookError::InvalidCommand(_))
        ));
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
