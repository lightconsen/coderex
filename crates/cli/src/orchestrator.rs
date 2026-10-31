//! Startup orchestrator — four-phase initialization.

use crate::cli::Cli;
use crate::daemon;
use crate::mcp_elicitation::SessionElicitationHandler;
use crate::session_resume;
use anyhow::{Context, Result, bail};
#[cfg(feature = "enterprise")]
use coderex_config::config_home;
use coderex_config::{ConfigService, RuntimeOverrides, Settings};
#[cfg(feature = "enterprise")]
use coderex_config::{
    LoadingBarrier, RemoteManagedSettings, RemoteManagedSettingsClient, RemoteSettingsManager,
};
use coderex_core::api_client::{ApiClient, EffortClient, ReasoningEffort};
use coderex_core::provider_config::ProviderConfig;
use coderex_core::providers::cloud::{CloudEndpoint, ProviderKind};
use coderex_core::query_profiles::ModelProfile;
use coderex_core::types::ModelId;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Instant;

/// Duration (ms) to wait for remote settings before timing out and failing open.
#[cfg(feature = "enterprise")]
const REMOTE_SETTINGS_TIMEOUT_MS: u64 = 5000;

/// Owns the background telemetry uploader task.
///
/// `TelemetryHandle::new` returns the uploader but nothing runs it; this guard
/// is the production caller. Dropping it aborts the task, so the uploader
/// cannot outlive the session that owns it.
#[derive(Debug)]
pub struct TelemetryUploaderGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl TelemetryUploaderGuard {
    /// Spawn an uploader onto the current tokio runtime.
    fn spawn(uploader: coderex_telemetry::TelemetryUploader) -> Self {
        Self {
            handle: tokio::spawn(async move {
                uploader.run().await;
            }),
        }
    }

    /// Whether the uploader task has stopped.
    ///
    /// A freshly spawned uploader is expected to be running; a `true` value
    /// means it exited (for example because the telemetry endpoint was
    /// disabled at runtime).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

impl Drop for TelemetryUploaderGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Records session start/end events and flushes the telemetry worker.
///
/// The guard holds its own handle clone so queued events survive the session
/// context's own handle being dropped. `shutdown` on drop flushes the worker's
/// pending batch; the event remains queued in SQLite for the next run if it
/// could not be uploaded in time.
#[derive(Debug)]
pub struct TelemetrySessionGuard {
    handle: coderex_telemetry::TelemetryHandle,
    session_id: String,
    started: Instant,
}

impl TelemetrySessionGuard {
    /// Record a session-start event with a fresh session id.
    fn start(handle: coderex_telemetry::TelemetryHandle) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        handle.session_start(session_id.clone());
        Self {
            handle,
            session_id,
            started: Instant::now(),
        }
    }
}

impl Drop for TelemetrySessionGuard {
    fn drop(&mut self) {
        self.handle
            .session_end(self.session_id.clone(), self.started.elapsed().as_secs());
        self.handle.shutdown();
    }
}

/// How many recent channel notifications the production subscriber keeps.
const PLUGIN_CHANNEL_EVENT_LOG: usize = 64;

/// The CLI's subscriber to plugin channel notifications.
///
/// A channel binds a plugin to a configured MCP server. The MCP manager routes
/// every notification that server emits into
/// [`PluginManager::route_channel_notification`](coderex_plugins::plugins::PluginManager::route_channel_notification),
/// which fans it out to the sinks subscribed to the channel. This is the
/// production sink: it records each delivery (bounded, oldest first out) and
/// reports it through `tracing`, so a notification a channel's server emits is
/// consumed rather than dropped. It is deliberately not a no-op — an
/// unsubscribed channel is exactly what makes `plugin_channel_report()` report
/// `Idle`.
#[derive(Debug)]
struct PluginChannelSink {
    log: Arc<
        std::sync::Mutex<std::collections::VecDeque<coderex_plugins::plugins::PluginChannelEvent>>,
    >,
}

impl coderex_plugins::plugins::ChannelSink for PluginChannelSink {
    fn deliver(&self, event: coderex_plugins::plugins::PluginChannelEvent) {
        tracing::info!(
            channel = %event.channel,
            plugin = %event.plugin,
            server = %event.server,
            method = %event.method,
            sequence = event.sequence,
            "plugin channel notification delivered"
        );
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.len() == PLUGIN_CHANNEL_EVENT_LOG {
            log.pop_front();
        }
        log.push_back(event);
    }
}

/// Resolved settings and state passed between startup phases.
#[derive(Debug, Default)]
pub struct SessionContext {
    /// Resolved settings after all merges
    /// (defaults → user → project → local → remote).
    pub settings: Option<Settings>,
    /// Remote managed settings (if fetched). Enterprise feature only.
    #[cfg(feature = "enterprise")]
    pub remote_settings: Option<RemoteManagedSettings>,
    /// Resolved model to use.
    pub model: Option<ModelId>,
    /// True when `--model` was given on the command line.
    ///
    /// A named custom agent may supply a model, but an explicit CLI choice is
    /// more specific and wins.
    pub model_explicit: bool,
    /// Loading barrier for remote settings (if configured). Enterprise feature only.
    #[cfg(feature = "enterprise")]
    pub remote_barrier: Option<LoadingBarrier>,
    /// MCP server connections initialized.
    pub mcp_ready: bool,
    /// Live MCP servers and their discovered tools, kept for the session so
    /// the tools can be registered into the tool registry.
    pub mcp_manager: Option<Arc<coderex_mcp::manager::McpManager>>,
    /// Answers questions MCP servers ask of the user, kept for the session.
    pub mcp_elicitation: Option<Arc<SessionElicitationHandler>>,
    /// True for `--print`, a piped prompt, or any session without a terminal.
    ///
    /// Server-initiated elicitation has nobody to ask in those modes, so the
    /// installed handler declines instead of reaching for stdin.
    pub non_interactive: bool,
    /// MCP-discovered skills registered during InitMcp.
    pub mcp_skills: Vec<coderex_plugins::skills::SkillMetadata>,
    /// Memory subsystem initialized.
    pub memory_ready: bool,
    /// Remote services initialized.
    pub remote_services_ready: bool,
    /// Working directory for the session.
    pub cwd: String,
    /// Plugin manager (initialized during InitPlugins).
    pub plugin_manager: Option<Arc<coderex_plugins::plugins::PluginManager>>,
    /// Host `userConfig` values from the `plugins` settings key.
    pub plugin_settings: Option<serde_json::Value>,
    /// Where plugin `.d.ts` declarations were exported, when any were.
    pub plugin_types_dir: Option<String>,
    /// Recent plugin channel notifications, recorded by the production
    /// subscriber (`subscribe_plugin_channels`).
    ///
    /// Bounded, oldest first out. Kept so a session can inspect what its
    /// channels actually delivered rather than only knowing a sink exists.
    pub plugin_channel_events: Arc<
        std::sync::Mutex<std::collections::VecDeque<coderex_plugins::plugins::PluginChannelEvent>>,
    >,
    /// Marketplaces configured in settings.
    pub marketplaces: Vec<coderex_config::MarketplaceSetting>,
    /// Telemetry handle (initialized during LoadSettings).
    pub telemetry_handle: Option<coderex_telemetry::TelemetryHandle>,
    /// Background uploader task, stopped when the session context is dropped.
    pub telemetry_uploader: Option<TelemetryUploaderGuard>,
    /// Session lifecycle guard (start/end events), dropped with the context.
    pub telemetry_session: Option<TelemetrySessionGuard>,
    /// Session name for transcript naming.
    pub session_name: Option<String>,
    /// Whether to skip permission prompts.
    pub skip_permissions: bool,
    /// Additional directories in context.
    pub add_dir: Vec<String>,
    /// Files included in context.
    pub file: Vec<std::path::PathBuf>,
    /// Custom system prompt.
    pub system_prompt: Option<String>,
    /// Text appended to the default system prompt.
    ///
    /// Independent of [`system_prompt`](Self::system_prompt): when both are
    /// set, the replacement supplies the base and this text still applies.
    pub append_system_prompt: Option<String>,
    /// Whether session persistence is disabled.
    pub no_persistence: bool,
    /// Whether slash commands are disabled.
    pub no_slash_commands: bool,
    /// Output format preference.
    pub output_format: String,
    /// Whether a headless run may negotiate tool approvals over stdin.
    ///
    /// Requires all three of `--print`, `--output-format stream-json`, and a
    /// positional prompt: the protocol sink exists to carry the request out,
    /// and stdin must be free to carry the answer in.
    pub headless_permission_control: bool,
    /// Debug file path.
    pub debug_file: Option<std::path::PathBuf>,
    /// Fallback model.
    pub fallback_model: Option<ModelId>,
    /// Allowed tools.
    pub allowed_tools: Vec<String>,
    /// Custom settings path.
    pub settings_path: Option<std::path::PathBuf>,
    /// MCP config path.
    pub mcp_config_path: Option<std::path::PathBuf>,
    /// MCP debug mode.
    pub mcp_debug: bool,
    /// Strict MCP config.
    pub strict_mcp_config: bool,
    /// Additional plugin directory.
    pub plugin_dir: Option<std::path::PathBuf>,
    /// Input format.
    pub input_format: String,
    /// JSON schema for structured output.
    pub json_schema: Option<String>,
    /// Maximum number of turns.
    pub max_turns: Option<u32>,
    /// Replay user messages.
    pub replay_user_messages: bool,
    /// Include partial messages.
    pub include_partial_messages: bool,
    /// Reasoning effort.
    pub effort: Option<String>,
    /// Beta features.
    pub betas: Vec<String>,
    /// Chrome integration.
    pub chrome: bool,
    /// No chrome.
    pub no_chrome: bool,
    /// Tmux integration.
    pub tmux: bool,
    /// Agent to use.
    pub agent: Option<String>,
    /// Worktree mode.
    pub worktree: bool,
    /// Fork from session.
    pub fork_session: Option<String>,
    /// Load from PR.
    pub from_pr: Option<String>,
    /// The rendered pull-request context fetched for `--from-pr`.
    pub from_pr_context: Option<String>,
    /// Show setting sources.
    pub setting_sources: bool,
    /// Verbose mode.
    pub verbose: bool,
    /// Debug mode.
    pub debug: bool,
    /// Bare output (no decorations).
    pub bare: bool,
    /// Brief output (concise).
    pub brief: bool,
}

/// Phases of startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupPhase {
    /// Core initialization (tracing, signal handlers).
    Bootstrap,
    /// Load config files (user + project settings).
    LoadSettings,
    /// Load remote-managed overrides.
    MergeRemoteManagedSettings,
    /// Build the capability set from resolved settings.
    BuildCapabilities,
    /// Register slash commands.
    LoadCommands,
    /// Register skills.
    LoadSkills,
    /// Initialize plugins.
    InitPlugins,
    /// Reconcile marketplace state.
    ReconcileMarketplaces,
    /// Initialize MCP servers (non-blocking).
    InitMcp,
    /// Initialize memory subsystem.
    InitMemory,
    /// Initialize remote services.
    InitRemoteServices,
    /// All initialization complete.
    Ready,
}

/// Orchestrates the startup sequence.
#[derive(Debug)]
pub struct StartupOrchestrator {
    /// Shared session state across phases.
    pub ctx: SessionContext,
}

impl Default for StartupOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a tool declared a real output schema.
///
/// An absent schema (`Null`) and an empty object both mean "untyped"; either
/// is rendered as `unknown` rather than as an object with no properties.
fn schema_is_present(schema: &serde_json::Value) -> bool {
    schema.as_object().is_some_and(|object| !object.is_empty())
}

/// Build a plugin manager, taking the signing key from the environment.
///
/// The key is the trust anchor for plugin signatures, so it comes from the
/// process environment — a source a repository cannot write — rather than from
/// anything a plugin package carries.
fn plugin_manager_with_signing_key() -> coderex_plugins::plugins::PluginManager {
    let mut manager = coderex_plugins::plugins::PluginManager::new();
    if manager.set_public_key_from_env() {
        tracing::info!(
            "plugin signature verification enabled from {}",
            coderex_plugins::plugins::PLUGIN_PUBLIC_KEY_ENV
        );
    } else {
        tracing::debug!(
            "no {} set — plugin signatures are not checked and installs are marked unverified",
            coderex_plugins::plugins::PLUGIN_PUBLIC_KEY_ENV
        );
    }
    manager
}

/// Whether a headless run may enable the stdin tool-approval channel.
///
/// All three conditions matter: the protocol sink only exists for
/// `stream-json`, `--print` is the only headless mode, and a prompt read from
/// stdin leaves stdin unavailable for `control_response` lines.
fn headless_permission_control_allowed(
    print: bool,
    output_format: &str,
    prompt_from_stdin: bool,
) -> bool {
    print && output_format == "stream-json" && !prompt_from_stdin
}

/// Merge `patch` into `base`, keeping `base` wherever it holds a value.
///
/// This is the source-precedence rule for plugin settings: `base` is the
/// settings the user already resolved, so anything it set wins. A key `base`
/// left `null` (the user did not set it) takes the patch's value; nested
/// objects merge key by key rather than one whole object replacing another.
fn deep_merge_missing(base: serde_json::Value, patch: serde_json::Value) -> serde_json::Value {
    match (base, patch) {
        (serde_json::Value::Object(mut base_map), serde_json::Value::Object(patch_map)) => {
            for (key, patch_value) in patch_map {
                match base_map.remove(&key) {
                    Some(base_value) => {
                        base_map.insert(key, deep_merge_missing(base_value, patch_value));
                    }
                    None => {
                        base_map.insert(key, patch_value);
                    }
                }
            }
            serde_json::Value::Object(base_map)
        }
        // The user did not set this key; the plugin may.
        (serde_json::Value::Null, patch) => patch,
        // The user set it; the plugin may not override it.
        (base, _) => base,
    }
}

/// How restrictive a permission mode is; lower is stricter.
///
/// Used to hold a plugin's `permission_mode` to "tighten only": a plugin may
/// move toward `plan` but never toward `bypass-permissions`.
fn permission_mode_restrictiveness(mode: &str) -> Option<u8> {
    match mode {
        "plan" => Some(0),
        "dont-ask" => Some(1),
        "default" | "auto" => Some(2),
        "accept-edits" => Some(3),
        "bypass-permissions" => Some(4),
        _ => None,
    }
}

/// Apply the tighten-only keys of a plugin settings patch.
///
/// `permission_mode` may only move toward a stricter mode, `telemetry_enabled`
/// may only be turned off, and `max_budget_usd` may only lower the ceiling. A
/// `telemetry` sub-configuration is never taken: a plugin has no business
/// choosing where a user's telemetry goes.
fn apply_tighten_only_settings(
    settings: &mut Settings,
    object: &serde_json::Map<String, serde_json::Value>,
) {
    if let Some(mode) = object
        .get("permission_mode")
        .and_then(serde_json::Value::as_str)
    {
        let requested = permission_mode_restrictiveness(mode);
        let current = settings
            .permission_mode
            .as_deref()
            .and_then(permission_mode_restrictiveness);
        match (requested, current) {
            (Some(requested), Some(current)) if requested < current => {
                settings.permission_mode = Some(mode.to_string());
            }
            (Some(_), None) => settings.permission_mode = Some(mode.to_string()),
            (Some(_), Some(_)) => tracing::warn!(
                mode = %mode,
                "plugin settings may not loosen the permission mode; ignoring"
            ),
            (None, _) => tracing::warn!(
                mode = %mode,
                "plugin settings name an unknown permission mode; ignoring"
            ),
        }
    }

    if let Some(enabled) = object
        .get("telemetry_enabled")
        .and_then(serde_json::Value::as_bool)
    {
        if enabled {
            tracing::warn!("plugin settings may not enable telemetry; ignoring");
        } else {
            settings.telemetry_enabled = Some(false);
        }
    }
    if object.contains_key("telemetry") {
        tracing::warn!("plugin settings may not configure telemetry; ignoring");
    }

    if let Some(budget) = object
        .get("max_budget_usd")
        .and_then(serde_json::Value::as_f64)
    {
        settings.max_budget_usd = Some(
            settings
                .max_budget_usd
                .map_or(budget, |existing| existing.min(budget)),
        );
    }
}

/// Plugin `settings` keys a plugin may never set, whatever the value.
///
/// A plugin that could install a hook, start an MCP server, set an API key, or
/// inject an environment variable would run with the user's authority rather
/// than beside it.
const PLUGIN_SETTINGS_NEVER_TAKEN: &[&str] = &[
    "hooks",
    "hook_layers",
    "mcpServers",
    "mcp_servers",
    "api_key",
    "env",
    "provider",
    "config_version",
    // A plugin that could grant itself allow rules would be approving its own
    // tool calls with the user's authority.
    "permissions",
];

/// Plugin `settings` keys a plugin may only make stricter.
const PLUGIN_SETTINGS_TIGHTEN_ONLY: &[&str] = &[
    "permission_mode",
    "telemetry_enabled",
    "telemetry",
    "max_budget_usd",
];

/// A [`Settings`] with every optional field populated.
///
/// Used only to enumerate the keys the resolved settings understands. A default
/// value would not do: `context_window`, `vim_mode` and `statusline` are
/// `skip_serializing_if = "Option::is_none"`, so they are absent from a default
/// serialization and would be missed by the enumeration.
fn settings_with_every_key() -> Settings {
    Settings {
        model: Some(String::new()),
        permission_mode: Some(String::new()),
        max_budget_usd: Some(0.0),
        telemetry_enabled: Some(false),
        telemetry: Some(coderex_config::settings::TelemetrySubConfig::default()),
        hooks: Some(serde_json::Value::Null),
        mcp_servers: Some(serde_json::Value::Null),
        api_key: Some(String::new()),
        provider: Some(coderex_config::ProviderSettings {
            base_url: Some(String::new()),
            request_timeout_secs: Some(1),
        }),
        env: Some(std::collections::BTreeMap::from([(
            "CODEREX_MODEL".to_string(),
            "m".to_string(),
        )])),
        context_window: Some(coderex_config::settings::ContextWindowSettings::default()),
        vim_mode: Some(false),
        statusline: Some(coderex_config::settings::StatuslineSettings::default()),
        prompt_caching: Some(false),
        permissions: Some(coderex_config::PermissionsSettings::default()),
        ..Settings::default()
    }
}

/// The keys a plugin `settings` document may merge into the resolved settings:
/// every key [`Settings`] understands, less the ones the never-taken and
/// tighten-only rules already govern.
///
/// Derived from `Settings` rather than listed by hand so the rule "every other
/// key the resolved settings understands is merged" stays true when the struct
/// gains a field. The two policy sets are applied first, so a key they govern
/// is never merged even though it appears here.
fn mergeable_settings_keys() -> std::collections::HashSet<String> {
    serde_json::to_value(settings_with_every_key())
        .ok()
        .and_then(|value| value.as_object().cloned())
        .map(|object| {
            object
                .keys()
                .filter(|key| {
                    !PLUGIN_SETTINGS_NEVER_TAKEN.contains(&key.as_str())
                        && !PLUGIN_SETTINGS_TIGHTEN_ONLY.contains(&key.as_str())
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Routes MCP server notifications into the plugin channel layer.
///
/// The MCP crate does not depend on the plugin crate, so the two meet here:
/// installed on the MCP manager through
/// [`McpManager::set_notification_router`](coderex_mcp::manager::McpManager::set_notification_router),
/// this hands every notification a configured server emits to
/// [`PluginManager::route_channel_notification`](coderex_plugins::plugins::PluginManager::route_channel_notification),
/// which delivers it to the sinks subscribed to a channel bound to that
/// server. Without it an MCP notification stops at the MCP manager and the
/// channel layer never sees it.
#[derive(Debug)]
struct PluginChannelRouter {
    manager: Arc<coderex_plugins::plugins::PluginManager>,
}

impl coderex_mcp::manager::ServerNotificationRouter for PluginChannelRouter {
    fn route(&self, server: &str, method: &str, params: Option<&serde_json::Value>) {
        let delivered = self
            .manager
            .route_channel_notification(server, method, params);
        if delivered > 0 {
            tracing::debug!(
                server = %server,
                method = %method,
                delivered,
                "MCP notification routed to plugin channels"
            );
        }
    }
}

impl StartupOrchestrator {
    /// Create a new orchestrator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ctx: SessionContext {
                settings: None,
                #[cfg(feature = "enterprise")]
                remote_settings: None,
                model: None,
                model_explicit: false,
                #[cfg(feature = "enterprise")]
                remote_barrier: None,
                mcp_ready: false,
                mcp_manager: None,
                mcp_elicitation: None,
                non_interactive: false,
                mcp_skills: Vec::new(),
                memory_ready: false,
                remote_services_ready: false,
                cwd: String::new(),
                plugin_manager: None,
                plugin_settings: None,
                plugin_types_dir: None,
                plugin_channel_events: Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::new(),
                )),
                marketplaces: Vec::new(),
                telemetry_handle: None,
                telemetry_uploader: None,
                telemetry_session: None,
                session_name: None,
                skip_permissions: false,
                add_dir: Vec::new(),
                file: Vec::new(),
                system_prompt: None,
                append_system_prompt: None,
                no_persistence: false,
                no_slash_commands: false,
                output_format: "text".into(),
                headless_permission_control: false,
                debug_file: None,
                fallback_model: None,
                allowed_tools: Vec::new(),
                settings_path: None,
                mcp_config_path: None,
                mcp_debug: false,
                strict_mcp_config: false,
                plugin_dir: None,
                input_format: "text".into(),
                json_schema: None,
                max_turns: None,
                replay_user_messages: false,
                include_partial_messages: false,
                effort: None,
                betas: Vec::new(),
                chrome: false,
                no_chrome: false,
                tmux: false,
                agent: None,
                worktree: false,
                fork_session: None,
                from_pr: None,
                from_pr_context: None,
                setting_sources: false,
                verbose: false,
                debug: false,
                bare: false,
                brief: false,
            },
        }
    }

    /// Run the full startup sequence.
    // The future borrows the non-`Sync` REPL loop, so it is not `Send`; `main`
    // awaits it on the current task and never spawns it onto another thread.
    #[allow(clippy::future_not_send)]
    pub async fn run(&mut self, args: Cli) -> Result<()> {
        // `--tmux` runs before any startup work. Outside tmux this replaces
        // the process with a tmux session running the same binary; inside
        // tmux it only sets the titles and returns.
        crate::tmux::apply(args.tmux)?;

        // No provider request in this workspace carries a reasoning-effort
        // field. Accepting the flag and silently ignoring it would make
        // `--effort high` look effective when it is not, so fail before any
        // startup work happens.
        Self::validate_effort(args.effort.as_deref())?;

        // Store working directory early
        // Recorded before settings resolution folds the CLI value into the
        // effective model, so `--agent` can tell an explicit choice apart.
        self.ctx.model_explicit = args.model.is_some();
        self.ctx.cwd = args.directory.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

        // Store CLI-derived context
        self.ctx.session_name.clone_from(&args.name);
        self.ctx.skip_permissions = args.dangerously_skip_permissions;
        self.ctx.add_dir.clone_from(&args.add_dir);
        self.ctx.file.clone_from(&args.file);
        self.ctx.system_prompt.clone_from(&args.system_prompt);
        self.ctx
            .append_system_prompt
            .clone_from(&args.append_system_prompt);
        self.ctx.no_persistence = args.no_session_persistence;
        self.ctx.no_slash_commands = args.disable_slash_commands;
        self.ctx.output_format.clone_from(&args.output_format);
        self.ctx.debug_file.clone_from(&args.debug_file);
        self.ctx.settings_path.clone_from(&args.settings);
        self.ctx.mcp_config_path.clone_from(&args.mcp_config);
        self.ctx.mcp_debug = args.mcp_debug;
        self.ctx.strict_mcp_config = args.strict_mcp_config;
        self.ctx.plugin_dir.clone_from(&args.plugin_dir);
        self.ctx.input_format.clone_from(&args.input_format);
        self.ctx.json_schema.clone_from(&args.json_schema);
        self.ctx.max_turns = args.max_turns;
        self.ctx.replay_user_messages = args.replay_user_messages;
        self.ctx.include_partial_messages = args.include_partial_messages;
        self.ctx.effort.clone_from(&args.effort);
        self.ctx.betas.clone_from(&args.betas);
        self.ctx.chrome = args.chrome;
        self.ctx.no_chrome = args.no_chrome;
        self.ctx.tmux = args.tmux;
        self.ctx.agent.clone_from(&args.agent);
        // Resolve the name before any startup phase runs. A typo must fail
        // loudly with the configured names, not start a default session that
        // silently ignores the flag.
        if let Some(ref name) = self.ctx.agent
            && let Err(error) = crate::agent_selection::resolve_agent(name)
        {
            bail!("{error}");
        }
        self.ctx.worktree = args.worktree;
        self.ctx.fork_session.clone_from(&args.fork_session);
        self.ctx.from_pr.clone_from(&args.from_pr);
        // Fetch once, before the REPL exists: the headless path prepends the
        // block to the prompt, the interactive path adds it as extra context,
        // and either way a failure stops startup with `gh`'s message instead
        // of pretending the PR was loaded.
        if let Some(ref url) = args.from_pr {
            let context = crate::from_pr::fetch(url)?;
            tracing::info!(
                url = %context.url,
                bytes = context.block.len(),
                "loaded pull-request context"
            );
            self.ctx.from_pr_context = Some(context.block);
        }
        self.ctx.setting_sources = args.setting_sources;
        self.ctx.verbose = args.verbose;
        self.ctx.debug = args.debug;
        self.ctx.bare = args.bare;
        self.ctx.brief = args.brief;
        if let Some(ref tools) = args.allowed_tools {
            self.ctx.allowed_tools = tools.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Some(ref fb) = args.fallback_model {
            self.ctx.fallback_model = Some(ModelId(fb.clone()));
        }
        // Decided before the phases run because the MCP servers are started
        // there: the handler they get has to know whether a person is there to
        // answer it.
        self.ctx.non_interactive =
            args.print || !args.prompt.is_empty() || !std::io::stdin().is_terminal();
        // The control channel needs a protocol sink to ask on and a free
        // stdin to answer on. Both hold only for `--print` with `stream-json`
        // output and a positional prompt; a prompt read from stdin leaves
        // stdin busy, and interactive mode has its own confirmation UI.
        self.ctx.headless_permission_control = headless_permission_control_allowed(
            args.print,
            &args.output_format,
            args.prompt.is_empty(),
        );

        self.run_phase(StartupPhase::Bootstrap, &args).await?;
        self.run_phase(StartupPhase::LoadSettings, &args).await?;
        #[cfg(feature = "enterprise")]
        self.run_phase(StartupPhase::MergeRemoteManagedSettings, &args)
            .await?;
        self.run_phase(StartupPhase::BuildCapabilities, &args)
            .await?;
        self.run_phase(StartupPhase::LoadCommands, &args).await?;
        self.run_phase(StartupPhase::LoadSkills, &args).await?;
        self.run_phase(StartupPhase::InitPlugins, &args).await?;
        self.run_phase(StartupPhase::ReconcileMarketplaces, &args)
            .await?;
        self.run_phase(StartupPhase::InitMcp, &args).await?;
        self.run_phase(StartupPhase::InitMemory, &args).await?;
        self.run_phase(StartupPhase::InitRemoteServices, &args)
            .await?;
        self.run_phase(StartupPhase::Ready, &args).await?;

        // Non-blocking update check (once per 24h)
        {
            let cwd = self.ctx.cwd.clone();
            tokio::spawn(async move {
                use crate::version;
                let cwd_path = std::path::Path::new(&cwd);
                if version::should_check(cwd_path) {
                    if let Some(msg) = version::check_for_updates().await {
                        tracing::info!("{msg}");
                    }
                    version::record_check_time(cwd_path);
                }
            });
        }

        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let cwd = self.ctx.cwd.clone();
            tokio::spawn(async move {
                let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
                    return;
                };
                let Ok(mut sighup) = signal(SignalKind::hangup()) else {
                    return;
                };
                loop {
                    tokio::select! {
                        _ = sigterm.recv() => {
                            if crate::repl::TUI_ACTIVE
                                .load(std::sync::atomic::Ordering::SeqCst)
                            {
                                // The TUI owns the terminal: only it can leave
                                // the alternate screen and raw mode. Its own
                                // SIGTERM branch performs the cleanup and exits
                                // with 143.
                                continue;
                            }
                            tracing::info!("Received SIGTERM — shutting down gracefully");
                            let _ = crate::daemon::remove_pid_file();
                            // Save recovery snapshot before exit
                            let _ = crate::recovery::remove(&cwd);
                            // `process::exit` runs no destructors, so plugin
                            // monitors are killed explicitly.
                            crate::plugin_monitors::terminate_all_monitors();
                            std::process::exit(0);
                        }
                        _ = sighup.recv() => {
                            tracing::info!("Received SIGHUP — config reload requested");
                            // Config reload is handled by the REPL's settings-file watcher
                        }
                    }
                }
            });
        }

        tracing::info!("CodeRex ready");

        // Determine model: CODEREX_MODEL / ANTHROPIC_MODEL / OPENAI_MODEL env
        // → --model/settings → default. `fast_mode` only changes the default:
        // an explicitly configured model (`--model`, the model variables, or a
        // settings `model` value) always wins, so the flag is what you get
        // when nothing else chose one.
        let explicit_model = std::env::var("CODEREX_MODEL")
            .or_else(|_| std::env::var("ANTHROPIC_MODEL"))
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .ok()
            .filter(|m| !m.is_empty())
            .map(ModelId)
            .or_else(|| self.ctx.model.clone());
        // `--betas` is a real overlay on the environment-provided flags, and
        // the session model is resolved here — before the REPL applies it —
        // so it has to be folded in for `--betas +fast_mode` to mean anything.
        // A malformed list is the REPL's own `--betas` handling's problem; here
        // a failure just leaves the environment-provided flags in place.
        let mut flags = coderex_config::flags::FeatureFlags::from_env();
        if !args.betas.is_empty() {
            let _ = flags.apply_comma_list(&args.betas.join(","));
        }
        let model = Self::resolve_session_model(explicit_model, flags.fast_mode);

        // Handle --resume flag (resume last or specific session)
        if let Some(ref session_id) = args.resume {
            return self.run_resume(&self.ctx.cwd, session_id, model).await;
        }
        if args.r#continue {
            // Continue most recent session — find latest transcript
            if let Some(latest) = self.find_latest_session() {
                return self.run_resume(&self.ctx.cwd, &latest, model).await;
            }
            bail!("No previous session found to continue");
        }

        // Route based on CLI args
        if args.print || !args.prompt.is_empty() {
            // Non-interactive: read from stdin or use positional prompt
            let prompt = if args.prompt.is_empty() {
                let mut stdin = String::new();
                std::io::stdin().read_line(&mut stdin)?;
                stdin.trim().to_string()
            } else {
                args.prompt.join(" ")
            };
            if prompt.is_empty() {
                bail!("No input provided");
            }
            self.run_headless_with_prompt(&self.ctx.cwd, model, &prompt)
                .await?;
            Ok(())
        } else if let Some(cmd) = args.command {
            match cmd {
                crate::cli::Command::Repl => self.run_repl(&self.ctx.cwd, model).await,
                crate::cli::Command::Serve { port } => self.run_server(&self.ctx.cwd, port).await,
                crate::cli::Command::ServeStop => self.run_serve_stop().await,
                crate::cli::Command::ServeStatus => self.run_serve_status().await,
                crate::cli::Command::Agents { detailed } => self.run_agents(detailed).await,
                crate::cli::Command::Auth => self.run_auth().await,
                crate::cli::Command::Doctor => self.run_doctor().await,
                crate::cli::Command::Update { check } => self.run_update(check).await,
                crate::cli::Command::TeleportService { port, ttl_secs } => {
                    run_teleport_service(port, ttl_secs).await
                }
                crate::cli::Command::Gateway { port } => run_gateway(port).await,
                crate::cli::Command::PluginTypecheck { dir } => {
                    run_plugin_typecheck(dir);
                    Ok(())
                }
                crate::cli::Command::Mcp { action } => self.run_mcp(action).await,
                crate::cli::Command::Plugin { action } => self.run_plugin(action).await,
                crate::cli::Command::AutoMode => self.run_auto_mode().await,
                crate::cli::Command::Telemetry { action } => self.run_telemetry(action).await,
                crate::cli::Command::Bridge(args) => {
                    crate::bridge_launcher::run(args, std::path::Path::new(&self.ctx.cwd)).await
                }
            }
        } else {
            // Interactive REPL
            self.run_repl(&self.ctx.cwd, model).await
        }
    }

    /// Run a single startup phase.
    async fn run_phase(&mut self, phase: StartupPhase, args: &Cli) -> Result<()> {
        match phase {
            StartupPhase::Bootstrap => self.phase_bootstrap().await,
            StartupPhase::LoadSettings => self.phase_load_settings(args).await,
            #[cfg(feature = "enterprise")]
            StartupPhase::MergeRemoteManagedSettings => self.phase_merge_remote_settings().await,
            #[cfg(not(feature = "enterprise"))]
            StartupPhase::MergeRemoteManagedSettings => {
                tracing::debug!("remote settings phase skipped (enterprise feature not enabled)");
                Ok(())
            }
            StartupPhase::BuildCapabilities => self.phase_build_capabilities().await,
            StartupPhase::LoadCommands => self.phase_load_commands().await,
            StartupPhase::LoadSkills => self.phase_load_skills().await,
            StartupPhase::InitPlugins => self.phase_init_plugins().await,
            StartupPhase::ReconcileMarketplaces => self.phase_reconcile_marketplaces().await,
            StartupPhase::InitMcp => self.phase_init_mcp().await,
            StartupPhase::InitMemory => self.phase_init_memory().await,
            StartupPhase::InitRemoteServices => self.phase_init_remote_services().await,
            StartupPhase::Ready => self.phase_ready().await,
        }
    }

    /// Bootstrap: initialize tracing, signal handlers, and core subsystems.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_bootstrap(&self) -> Result<()> {
        // Build log level from CLI flags: --debug → debug, --verbose → debug, default → info
        let log_level = if self.ctx.debug || self.ctx.verbose {
            "debug"
        } else {
            "info"
        };
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level));

        // Initialize tracing if not already done
        let subscriber: Box<dyn tracing::Subscriber + Send + Sync> =
            if let Some(ref debug_path) = self.ctx.debug_file {
                if let Some(parent) = debug_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(debug_path)
                    .unwrap_or_else(|_| std::fs::File::open("/dev/null").unwrap());
                Box::new(
                    tracing_subscriber::fmt()
                        .with_env_filter(filter)
                        .with_writer(move || {
                            file.try_clone()
                                .unwrap_or_else(|_| std::fs::File::open("/dev/null").unwrap())
                        })
                        .finish(),
                )
            } else {
                Box::new(tracing_subscriber::fmt().with_env_filter(filter).finish())
            };

        // Only init once — ignore if already initialized
        let _ = tracing::subscriber::set_global_default(subscriber);

        tracing::info!("bootstrap complete");
        Ok(())
    }

    /// LoadSettings: load user + project config, resolve with overrides,
    /// and initialize telemetry with settings.json integration.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_load_settings(&mut self, args: &Cli) -> Result<()> {
        let config_dir = coderex_config::config_home();

        // Use custom settings path if --settings is provided
        let user_path = args
            .settings
            .clone()
            .unwrap_or_else(|| config_dir.join("settings.json"));

        // Find project config in CWD or ancestors
        let project_path = Self::find_project_config(&self.ctx.cwd);

        // The managed-settings file carrier (enterprise). Defaults to
        // <config_dir>/managed_settings.json; deployments can point it at an
        // admin-controlled location via the env var.
        let managed_path = std::env::var("CODEREX_MANAGED_SETTINGS_PATH").map_or_else(
            |_| config_dir.join("managed_settings.json"),
            std::path::PathBuf::from,
        );

        // Platform carriers are attached after the file carrier: an explicit
        // `--settings`/environment file path keeps precedence, and the OS
        // policy carrier is added only when it was not explicitly configured.
        let config_service = ConfigService::new(user_path.clone(), project_path)
            .with_local_config_path(Self::local_config_path(&self.ctx.cwd))
            .with_managed_settings_path(managed_path)
            .with_platform_managed_settings();
        // Explicit environment overrides for the platform carriers exist for
        // deployments and tests that need a non-default location.
        let config_service = match std::env::var("CODEREX_MANAGED_SETTINGS_PLIST_PATH") {
            Ok(path) => config_service.with_managed_settings_plist(path.into()),
            Err(_) => config_service,
        };
        let config_service = match std::env::var("CODEREX_MANAGED_SETTINGS_REGISTRY_KEY") {
            Ok(key) => config_service.with_managed_settings_registry_key(key),
            Err(_) => config_service,
        };
        let mut settings = config_service.resolve()?;
        let plugin_host = config_service.load_plugin_host_config();
        self.ctx.plugin_settings = plugin_host.plugins;
        self.ctx.marketplaces = plugin_host.marketplaces;

        // First-run consent gate. Telemetry is opt-in: `telemetry_enabled` is
        // `None` until the user decides (the built-in defaults no longer force
        // it on), so this gate is reachable. It fires on first run — no user
        // settings file yet and no decision recorded anywhere.
        if !user_path.exists()
            && settings.telemetry_enabled.is_none()
            && let Some(choice) = Self::prompt_telemetry_consent()
        {
            settings.telemetry_enabled = Some(choice);
            // Persist the decision on its own. Writing the resolved
            // settings back would copy everything that went into resolving
            // them — including whatever a project's `.coderex/settings.json`
            // contributed, which the repository controls — into the
            // person's permanent settings as a side effect of answering a
            // question about telemetry.
            let _ = Self::write_telemetry_consent(&user_path, choice);
        }

        // Apply CLI runtime overrides
        let overrides = RuntimeOverrides {
            model: args.model.clone(),
            permission_mode: args.permissions.clone(),
            max_budget_usd: args.max_budget_usd,
            telemetry_enabled: if args.no_telemetry { Some(false) } else { None },
            api_key: args.api_key.clone(),
            system_prompt: args.system_prompt.clone(),
            append_system_prompt: args.append_system_prompt.clone(),
            allowed_tools: args
                .allowed_tools
                .as_ref()
                .map(|s| s.split(',').map(|t| t.trim().to_string()).collect()),
            fallback_model: args.fallback_model.clone(),
            effort: args.effort.clone(),
        };
        // These values are session-scoped, not settings keys. Take them before
        // `apply` consumes the overrides so none of them is silently dropped.
        let session = overrides.session_overrides();
        settings = overrides.apply(settings);

        // Apply the Claude-compatible `env` block before anything below reads
        // the environment: model selection, providers, feature flags and the
        // telemetry channels all consult it later in startup.
        Self::apply_settings_env(&settings);

        if let Some(system_prompt) = session.system_prompt {
            self.ctx.system_prompt = Some(system_prompt);
        }
        self.ctx.append_system_prompt = session.append_system_prompt;
        if let Some(fallback_model) = session.fallback_model {
            self.ctx.fallback_model = Some(ModelId(fallback_model));
        }
        if let Some(allowed_tools) = session.allowed_tools
            && !allowed_tools.is_empty()
        {
            self.ctx.allowed_tools = allowed_tools;
        }
        Self::validate_effort(session.effort.as_deref())?;
        self.ctx.effort = session.effort;

        tracing::info!(
            "settings loaded: model={}, effective_model={}",
            settings.model.as_deref().unwrap_or("default"),
            std::env::var("CODEREX_MODEL")
                .or_else(|_| std::env::var("ANTHROPIC_MODEL"))
                .or_else(|_| std::env::var("OPENAI_MODEL"))
                .ok()
                .filter(|model| !model.is_empty())
                .or_else(|| settings.model.clone())
                .unwrap_or_else(|| "default".into())
        );

        // --setting-sources: print where each value came from
        if args.setting_sources {
            let diff = settings.diff_from_defaults("resolved");
            println!("{}", diff.render());
        }

        // Initialize telemetry subsystem with resolved settings.
        // Undecided (`None`) means no consent was recorded — stay off.
        let telemetry_enabled = settings.telemetry_enabled.unwrap_or(false);
        // The panic hook is installed before settings are known, so it starts
        // inert and is enabled here. A user who never opted in must not have
        // crash files written on their behalf.
        coderex_telemetry::set_capture_enabled(telemetry_enabled);
        // Pending crash files written by an earlier (enabled) run are pruned
        // on every startup. The TTL used to be enforced only inside
        // `ingest_pending`, which the disabled path never reaches.
        let pending_dir = coderex_config::config_home()
            .join("crashes")
            .join("pending");
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(
                coderex_telemetry::DEFAULT_PENDING_MAX_AGE_SECS,
            ))
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match coderex_telemetry::prune_pending(&pending_dir, cutoff) {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, "pruned expired pending crash reports"),
            Err(error) => tracing::warn!(
                directory = %pending_dir.display(),
                %error,
                "could not prune pending crash reports"
            ),
        }
        if telemetry_enabled {
            if let Some((handle, uploader)) = Self::init_telemetry(&settings) {
                self.ctx.telemetry_session = Some(TelemetrySessionGuard::start(handle.clone()));
                self.ctx.telemetry_uploader = Self::spawn_telemetry_uploader(uploader);
                self.ctx.telemetry_handle = Some(handle);
            }
        } else {
            tracing::info!("telemetry disabled by user preference");
        }

        self.ctx.settings = Some(settings);
        Ok(())
    }

    /// Start the uploader task when telemetry produced one.
    ///
    /// `TelemetryHandle::new` returns `None` for an opted-out configuration, so
    /// a disabled session has no task to start.
    fn spawn_telemetry_uploader(
        uploader: Option<coderex_telemetry::TelemetryUploader>,
    ) -> Option<TelemetryUploaderGuard> {
        uploader.map(TelemetryUploaderGuard::spawn)
    }

    /// Build export-channel configuration from resolved settings, falling back
    /// to the environment for values the settings file does not set.
    fn telemetry_channels(settings: &Settings) -> coderex_telemetry::ChannelConfig {
        Self::apply_telemetry_channel_settings(
            coderex_telemetry::ChannelConfig::from_env(),
            settings,
        )
    }

    /// Pure settings overlay for [`Self::telemetry_channels`].
    fn apply_telemetry_channel_settings(
        mut channels: coderex_telemetry::ChannelConfig,
        settings: &Settings,
    ) -> coderex_telemetry::ChannelConfig {
        if let Some(telemetry) = &settings.telemetry {
            let set = |slot: &mut Option<String>, value: &Option<String>| {
                if let Some(value) = value.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()) {
                    *slot = Some(value.to_string());
                }
            };
            set(&mut channels.otlp_endpoint, &telemetry.otlp_endpoint);
            if let Some(headers) = telemetry
                .otlp_headers
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                channels.otlp_headers = coderex_telemetry::parse_otlp_headers(Some(headers));
            }
            set(&mut channels.datadog_api_key, &telemetry.datadog_api_key);
            set(&mut channels.datadog_endpoint, &telemetry.datadog_endpoint);
            if let Some(site) = telemetry
                .datadog_site
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                channels.datadog_site = site.to_string();
            }
        }
        // Channels are constructed only for an opted-in session, and the
        // source policy has already dropped repository-controlled opt-ins.
        channels.enabled = true;
        channels
    }

    /// Initialize the telemetry subsystem using resolved settings.
    fn init_telemetry(
        settings: &Settings,
    ) -> Option<(
        coderex_telemetry::TelemetryHandle,
        Option<coderex_telemetry::TelemetryUploader>,
    )> {
        let config_home = coderex_config::config_home();
        let db_path = config_home.join("telemetry.db");
        let pending_dir = config_home.join("crashes").join("pending");

        let store = match coderex_telemetry::TelemetryStore::open(&db_path) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!("failed to open telemetry store: {e}");
                return None;
            }
        };

        // Ingest crash reports from previous panics
        let crash_reporter = coderex_telemetry::CrashReporter::new(pending_dir);
        match crash_reporter.ingest_pending(&store) {
            Ok(count) => {
                if count > 0 {
                    tracing::info!("ingested {count} pending crash reports");
                }
            }
            Err(e) => tracing::warn!("failed to ingest pending crashes: {e}"),
        }

        // Build TelemetryConfig from the settings sub-config. An opted-in
        // session with no sub-config still gets a working uploader with the
        // built-in defaults; otherwise `telemetry_enabled = true` would only
        // write to the local database.
        let telemetry_config = Some(Self::telemetry_config_from_settings(settings));

        // `TelemetryUploader::new` derives channels from the environment; the
        // settings file is the host's own configuration and must be able to
        // supply (or override) OTLP/Datadog values too.
        let channels = Self::telemetry_channels(settings);
        let has_endpoint = settings
            .telemetry
            .as_ref()
            .and_then(|telemetry| telemetry.endpoint.as_deref())
            .is_some_and(|endpoint| !endpoint.trim().is_empty());
        let has_destination = has_endpoint || channels.has_destination();
        let (handle, uploader) = coderex_telemetry::TelemetryHandle::new(&store, telemetry_config);
        let uploader = uploader.map(|uploader| uploader.with_channels(channels));
        // Opting in without a destination keeps the data local instead of
        // posting to an endpoint the project does not operate.
        let uploader = if has_destination {
            uploader
        } else {
            tracing::info!(
                "telemetry is enabled but no endpoint is configured; recording locally only"
            );
            None
        };
        Some((handle, uploader))
    }

    /// Prompt the user for telemetry consent on first run.
    ///
    /// Returns `Some(true/false)` if the user answered, or `None` if
    /// stdin is not interactive or the prompt failed.
    fn prompt_telemetry_consent() -> Option<bool> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        eprintln!();
        eprintln!("Help improve CodeRex by sending anonymous usage data?");
        eprintln!("  - No PII is collected");
        eprintln!(
            "  - Events are stored locally; they are uploaded only if you configure an endpoint"
        );
        eprintln!("  - You can change this anytime in ~/.coderex/settings.json");
        eprint!("Enable telemetry? [Y/n]: ");

        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            return None;
        }
        let trimmed = input.trim().to_lowercase();
        let choice = trimmed.is_empty() || trimmed.starts_with('y');
        Some(choice)
    }

    /// Record the telemetry decision, and nothing else.
    ///
    /// The consent gate runs before any settings file exists, so this is what
    /// creates one. It writes the one field it is answering for: a resolved
    /// `Settings` snapshot would carry in defaults and project values the
    /// person never chose.
    fn write_telemetry_consent(
        path: &std::path::Path,
        enabled: bool,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let document = serde_json::json!({
            "config_version": coderex_config::settings::CURRENT_SCHEMA_VERSION,
            "telemetry_enabled": enabled,
        });
        let json = serde_json::to_string_pretty(&document).unwrap_or_default();
        std::fs::write(path, json)
    }

    /// MergeRemoteManagedSettings: fetch remote settings with fail-open semantics.
    ///
    /// Uses a loading barrier so other subsystems can wait for remote settings
    /// without blocking the full startup. On timeout or error, fails open.
    #[cfg(feature = "enterprise")]
    async fn phase_merge_remote_settings(&mut self) -> Result<()> {
        let _settings = self.ctx.settings.clone().unwrap_or_default();

        // Build remote settings client from environment / config
        let base_url = std::env::var("CODEREX_REMOTE_SETTINGS_URL").unwrap_or_default();
        let api_key = std::env::var("CODEREX_REMOTE_API_KEY").unwrap_or_default();

        let mgr = RemoteSettingsManager::new(
            base_url,
            api_key.clone(),
            Some(config_home().join("remote_settings_cache.json")),
        );

        if mgr.client.is_some() {
            tracing::info!("remote settings client configured, fetching asynchronously");

            // Start async fetch in the background
            mgr.start_fetch().await;

            // Wait for barrier with timeout — fail open on timeout
            let barrier_result = tokio::time::timeout(
                std::time::Duration::from_millis(REMOTE_SETTINGS_TIMEOUT_MS),
                mgr.barrier.wait(),
            )
            .await;

            match barrier_result {
                Ok(state) => {
                    match state {
                        coderex_config::FetchState::Ready {
                            settings: remote, ..
                        } => {
                            tracing::info!("remote settings applied");
                            self.ctx.remote_settings = Some(remote);
                        }
                        coderex_config::FetchState::Failed { error, .. } => {
                            tracing::warn!("remote settings fetch failed (failing open): {error}");
                        }
                        _ => {
                            // Still fetching — we'll merge later when ready
                            tracing::debug!(
                                "remote settings still fetching, will merge asynchronously"
                            );
                        }
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        "remote settings timeout after {}ms (failing open)",
                        REMOTE_SETTINGS_TIMEOUT_MS
                    );
                }
            }

            // Merge remote into local settings
            if let Some(remote) = mgr.get_settings().await {
                let local = self.ctx.settings.clone().unwrap_or_default();
                let merged = RemoteManagedSettingsClient::merge_into_local(&remote, local);
                self.ctx.settings = Some(merged);
                self.ctx.remote_settings = Some(remote);
            }

            // Store barrier for later use by other subsystems
            self.ctx.remote_barrier = Some(mgr.barrier.clone());
        } else {
            tracing::debug!("no remote settings client configured, using local only");
            // Resolve immediately
            mgr.start_fetch().await;
        }

        Ok(())
    }

    /// BuildCapabilities: derive feature flags and capabilities from resolved settings.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_build_capabilities(&mut self) -> Result<()> {
        // Resolve model from settings. A settings file that never mentions
        // `model` resolves to the built-in default; copying that into
        // `ctx.model` would make it look explicitly chosen and would stop
        // `--betas +fast_mode` from selecting the lightweight profile.
        if let Some(ref settings) = self.ctx.settings {
            let model_is_explicit = settings
                .diff_from_defaults("resolved")
                .changes
                .iter()
                .any(|change| change.key == "model");
            if model_is_explicit && let Some(ref model_name) = settings.model {
                self.ctx.model = Some(ModelId(model_name.clone()));
            }
        }

        tracing::info!("capabilities built");
        Ok(())
    }

    /// LoadCommands: discover and register custom commands from `.coderex/commands/`.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_load_commands(&self) -> Result<()> {
        let commands_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/commands");
        if commands_dir.exists() {
            let mut count = 0;
            if let Ok(entries) = std::fs::read_dir(&commands_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "json")
                        && let Ok(content) = std::fs::read_to_string(&path)
                        && let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&content)
                        && let Some(name) = cmd.get("name").and_then(|n| n.as_str())
                    {
                        tracing::debug!("discovered custom command: {name}");
                        count += 1;
                    }
                }
            }
            tracing::info!("commands loaded: {count} custom from {commands_dir:?}");
        } else {
            tracing::debug!("no custom commands directory found");
        }
        Ok(())
    }

    /// LoadSkills: discover and register skills from .coderex/skills/.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_load_skills(&self) -> Result<()> {
        let skills_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/skills");
        if skills_dir.exists() {
            let count = std::fs::read_dir(&skills_dir).map_or(0, std::iter::Iterator::count);
            tracing::info!("skills loaded: {count} from {skills_dir:?}");
        } else {
            tracing::debug!("no skills directory found");
        }
        Ok(())
    }

    /// The builtin tool schemas the plugin type declaration is generated from.
    ///
    /// The session's dispatcher is built per query and is not reachable from
    /// plugin startup, so this mirrors the registration source both the REPL
    /// and the served engine use: [`coderex_tools::register_builtin_tools`].
    /// MCP and other dynamic tools only exist after a session connects, so
    /// they are not part of the generated declaration.
    fn builtin_tool_type_schemas() -> Vec<coderex_plugins::declarations::ToolTypeSchema> {
        let mut registry = coderex_tools::ToolRegistry::new();
        coderex_tools::register_builtin_tools(&mut registry);
        registry
            .all_descriptors()
            .into_iter()
            .map(|descriptor| coderex_plugins::declarations::ToolTypeSchema {
                name: descriptor.name,
                input_schema: descriptor.input_schema,
                output_schema: schema_is_present(&descriptor.output_schema)
                    .then_some(descriptor.output_schema),
            })
            .collect()
    }

    /// InitPlugins: discover, load, and spawn plugins from plugin directories.
    async fn phase_init_plugins(&mut self) -> Result<()> {
        let mut manager = plugin_manager_with_signing_key();
        let mut plugin_dirs: Vec<std::path::PathBuf> = Vec::new();

        // Project-local plugins
        let project_plugins = std::path::Path::new(&self.ctx.cwd).join(".coderex/plugins");
        if project_plugins.exists() {
            manager.add_plugin_dir(project_plugins.clone());
            plugin_dirs.push(project_plugins);
        }

        // Global plugins in config home
        let global_plugins = coderex_config::config_home().join("plugins");
        if global_plugins.exists() {
            manager.add_plugin_dir(global_plugins.clone());
            plugin_dirs.push(global_plugins);
        }

        // Additional plugin directory from --plugin-dir
        if let Some(ref extra_dir) = self.ctx.plugin_dir {
            if extra_dir.exists() {
                manager.add_plugin_dir(extra_dir.clone());
                plugin_dirs.push(extra_dir.clone());
            } else {
                tracing::warn!("plugin directory does not exist: {extra_dir:?}");
            }
        }

        // Discover and load each plugin
        let mut loaded = 0;
        for dir in &plugin_dirs {
            if !dir.exists() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let manifest_path = entry.path().join("plugin.json");
                    if manifest_path.exists() {
                        match std::fs::read_to_string(&manifest_path) {
                            Ok(content) => match serde_json::from_str::<
                                coderex_plugins::plugins::PluginManifest,
                            >(&content)
                            {
                                Ok(manifest) => {
                                    manager.load_plugin(manifest, entry.path());
                                    loaded += 1;
                                }
                                // `deny_unknown_fields` turns a misspelled
                                // field into a parse error. Dropping it in
                                // silence was how an author's field vanished:
                                // say which plugin and why.
                                Err(error) => tracing::warn!(
                                    plugin = %entry.path().display(),
                                    %error,
                                    "ignoring plugin: plugin.json is not a valid manifest"
                                ),
                            },
                            Err(error) => tracing::warn!(
                                plugin = %entry.path().display(),
                                %error,
                                "ignoring plugin: plugin.json could not be read"
                            ),
                        }
                    }
                }
            }
        }

        // Resolve host-provided `userConfig` option values before spawning so
        // each process receives them in its environment. `ctx.plugin_settings`
        // was filled by the settings phase; load it here as a fallback for a
        // caller that reached plugin init without that phase running.
        let plugin_settings = self.ctx.plugin_settings.clone().or_else(|| {
            let config_service = self.plugin_host_config_service();
            config_service.load_plugin_host_config().plugins
        });
        if let Some(plugins) = plugin_settings.as_ref() {
            match manager.apply_options_from_settings(plugins) {
                Ok(count) => tracing::info!("applied userConfig for {count} plugin option(s)"),
                Err(error) => tracing::warn!("plugin userConfig ignored: {error}"),
            }
        }

        // Spawn process plugins and discover commands/skills dynamically
        let mut spawned = 0;
        for name in manager.list_plugins() {
            match manager.spawn_plugin(&name).await {
                Ok(()) => {
                    spawned += 1;
                    tracing::info!(plugin = %name, "spawned process plugin");
                }
                Err(e) => {
                    tracing::warn!(plugin = %name, "failed to spawn plugin: {e}");
                }
            }
        }

        tracing::info!("plugins initialized: {loaded} loaded, {spawned} spawned");

        let manager = Arc::new(manager);
        // Plugin `settings` are raw, repository-writable input. Only the
        // non-security keys are taken, and only where the session has no
        // value yet, so a plugin cannot raise a permission mode, turn on
        // telemetry, or replace an explicitly configured model.
        if let Some(value) = manager.plugin_settings() {
            self.apply_plugin_settings(&value);
        }
        // Declared `.d.ts` files are exported so the declaration has a real
        // consumer (a stable include path for plugin authors). The same root
        // also receives the host's generated type surface: the builtin tool
        // schemas, the engine/hook shape, the MCP server shape, and a
        // `tsconfig.json` that includes every emitted directory.
        let tool_schemas = Self::builtin_tool_type_schemas();
        self.ctx.plugin_types_dir = match manager.export_plugin_types(
            &coderex_config::config_home().join("plugin-types"),
            &tool_schemas,
        ) {
            Ok(paths) if paths.is_empty() => None,
            Ok(paths) => {
                tracing::info!(count = paths.len(), "plugin type declarations exported");
                Some(
                    coderex_config::config_home()
                        .join("plugin-types")
                        .display()
                        .to_string(),
                )
            }
            Err(error) => {
                tracing::warn!(%error, "plugin type declarations were not exported");
                None
            }
        };
        // Exported, but until now never checked: a plugin could ship a
        // declaration with a syntax error and the only symptom was an editor
        // underline. The compile runs off the startup thread and only when a
        // compiler exists; "no compiler" is said rather than treated as a pass.
        if let Some(root) = self.ctx.plugin_types_dir.clone()
            && !Self::env_flag("CODEREX_PLUGIN_NO_TYPECHECK")
        {
            let root = std::path::PathBuf::from(root);
            tokio::spawn(async move {
                let outcome = tokio::task::spawn_blocking(move || {
                    coderex_plugins::typecheck::typecheck(
                        &root,
                        coderex_plugins::typecheck::DEFAULT_TIMEOUT,
                    )
                })
                .await;
                match outcome {
                    Ok(coderex_plugins::typecheck::TypeCheckOutcome::Failed(diagnostics)) => {
                        tracing::warn!(
                            count = diagnostics.len(),
                            first = %diagnostics[0].render(),
                            "plugin type declarations have errors"
                        );
                    }
                    Ok(coderex_plugins::typecheck::TypeCheckOutcome::Passed) => {
                        tracing::info!("plugin type declarations type-check");
                    }
                    Ok(coderex_plugins::typecheck::TypeCheckOutcome::TimedOut(limit)) => {
                        tracing::warn!(seconds = limit.as_secs(), "plugin type check timed out");
                    }
                    Ok(coderex_plugins::typecheck::TypeCheckOutcome::Unavailable(reason)) => {
                        tracing::debug!(%reason, "plugin type check unavailable");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "plugin type check task failed");
                    }
                }
            });
        }
        // Declared binaries are pinned by SHA-256; fetch them once, after the
        // plugins are active. Failures are warnings: a plugin can still work
        // without its helper.
        if !Self::env_flag("CODEREX_PLUGIN_NO_BINARIES") {
            for name in manager.list_plugins() {
                match manager.fetch_plugin_binaries(&name).await {
                    Ok(paths) if !paths.is_empty() => {
                        tracing::info!(plugin = %name, count = paths.len(), "plugin binaries ready");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        plugin = %name,
                        %error,
                        "plugin binaries were not fetched"
                    ),
                }
            }
        }
        // Error-state plugins are retried while the session runs. The loop
        // holds a Weak reference and exits when the manager is dropped; the
        // handle is detached deliberately.
        let _retry_loop =
            Self::spawn_plugin_restart_loop(&manager, std::time::Duration::from_secs(1));
        // The channel layer needs a production subscriber, or every channel
        // would report `Idle` forever and a routed notification would reach
        // nobody. Subscribe one sink to every declared channel and keep the log
        // it records into.
        self.ctx.plugin_channel_events = Self::subscribe_plugin_channels(&manager);
        self.ctx.plugin_manager = Some(manager);
        Ok(())
    }

    /// Merge the safe keys of a plugin `settings` document into the session.
    ///
    /// A plugin's `settings` is an untrusted patch over the settings the user
    /// already resolved. Three rules apply, in this order:
    ///
    /// 1. **Never taken.** `hooks`, `mcp_servers`/`mcpServers`, `api_key`,
    ///    `env`, `hook_layers` and `config_version` are refused outright with a
    ///    warning. A plugin that could install a hook, start an MCP server,
    ///    set an API key, or inject an environment variable would be running
    ///    with the user's authority rather than beside it.
    /// 2. **Tighten-only.** `permission_mode`, `telemetry_enabled` and
    ///    `max_budget_usd` may only make the session stricter: a plugin can
    ///    move the permission mode toward read-only, turn telemetry *off*, or
    ///    lower the spending ceiling — never the reverse.
    /// 3. **Deep merge, user wins.** Every other key the resolved [`Settings`]
    ///    understands is merged into the current value with the user's value
    ///    taking precedence: the plugin fills only the gaps, recursively for
    ///    nested objects. A key CodeRex does not know is warned about and
    ///    ignored.
    ///
    /// The mergeable set is [`mergeable_settings_keys`], derived from
    /// [`Settings`] itself: it is every key the resolved settings understands
    /// except the two policy sets above, so a key added to `Settings` is
    /// mergeable the day it lands rather than the day someone remembers to add
    /// it to a hand-written list.
    ///
    /// Precedence is by *source*, not by plugin order: user and project
    /// settings outrank every plugin, so a plugin can supply a default the
    /// user never set but can never override one they did.
    fn apply_plugin_settings(&mut self, value: &serde_json::Value) {
        let mergeable = mergeable_settings_keys();

        let Some(object) = value.as_object() else {
            tracing::warn!("plugin settings are not an object; ignoring them");
            return;
        };

        let refused: Vec<&str> = object
            .keys()
            .filter(|key| PLUGIN_SETTINGS_NEVER_TAKEN.contains(&key.as_str()))
            .map(String::as_str)
            .collect();
        if !refused.is_empty() {
            tracing::warn!(
                refused = ?refused,
                "plugin settings may not set security-sensitive keys; ignoring them"
            );
        }

        let mut patch = serde_json::Map::new();
        let mut unknown = Vec::new();
        for (key, entry) in object {
            if PLUGIN_SETTINGS_NEVER_TAKEN.contains(&key.as_str())
                || PLUGIN_SETTINGS_TIGHTEN_ONLY.contains(&key.as_str())
            {
                continue;
            }
            if mergeable.contains(key.as_str()) {
                patch.insert(key.clone(), entry.clone());
            } else {
                unknown.push(key.as_str());
            }
        }
        if !unknown.is_empty() {
            tracing::warn!(
                ignored = ?unknown,
                "plugin settings contain keys CodeRex does not accept from plugins"
            );
        }

        let mut settings = self.ctx.settings.clone().unwrap_or_default();
        // Merge at the JSON level so nested objects (context_window,
        // statusline) merge key by key instead of one whole object replacing
        // another. `hook_layers` is `#[serde(skip)]`, so it is restored after
        // the round-trip rather than lost.
        let base = serde_json::to_value(&settings).unwrap_or(serde_json::Value::Null);
        let merged = deep_merge_missing(base, serde_json::Value::Object(patch));
        match serde_json::from_value::<Settings>(merged) {
            Ok(parsed) => {
                let hook_layers = settings.hook_layers.take();
                settings = parsed;
                settings.hook_layers = hook_layers;
            }
            Err(error) => {
                tracing::warn!(%error, "plugin settings could not be parsed; ignoring them");
                return;
            }
        }

        apply_tighten_only_settings(&mut settings, object);

        if self.ctx.model.is_none() && !self.ctx.model_explicit {
            let model_is_explicit = settings
                .diff_from_defaults("resolved")
                .changes
                .iter()
                .any(|change| change.key == "model");
            if model_is_explicit {
                self.ctx.model = settings.model.clone().map(coderex_core::types::ModelId);
            }
        }
        self.ctx.settings = Some(settings);
    }

    /// The telemetry configuration this session's uploader is built from.
    ///
    /// A pure function of the settings so the seam can be tested: the strict
    /// flag it carries is what makes the uploader redact, and a test that
    /// builds the uploader from this value exercises the same path a session
    /// does.
    fn telemetry_config_from_settings(settings: &Settings) -> coderex_telemetry::TelemetryConfig {
        let api_key = settings
            .api_key
            .as_ref()
            .map(|k| coderex_config::resolve_api_key(k));
        let defaults = coderex_telemetry::TelemetryConfig::default();
        let t = settings.telemetry.as_ref();
        coderex_telemetry::TelemetryConfig {
            // No destination ships with the project: an opted-in session with
            // no endpoint configured records locally only.
            endpoint: t.and_then(|t| t.endpoint.clone()).unwrap_or_default(),
            crash_endpoint: t.and_then(|t| t.crash_endpoint.clone()),
            api_key,
            batch_size: t.and_then(|t| t.batch_size).unwrap_or(defaults.batch_size),
            upload_interval_secs: t
                .and_then(|t| t.upload_interval_secs)
                .unwrap_or(defaults.upload_interval_secs),
            enabled: true,
            sample_rate: t
                .and_then(|t| t.sample_rate)
                .unwrap_or(defaults.sample_rate),
            pinned_cert_pem: t.and_then(|t| t.pinned_cert_pem.clone()),
            // Settings are a second trigger for the strict mode; the uploader
            // ORs this with `CODEREX_HIPAA`, so neither source can turn the
            // stricter redaction off.
            strict: t.and_then(|t| t.strict).unwrap_or(false),
        }
    }

    /// The redaction modes the uploader would resolve for this session.
    ///
    /// `strict` is the OR of the settings key and `CODEREX_HIPAA`, exactly as
    /// `TelemetryUploader::new` resolves it, so `telemetry preview-crash`
    /// prints the same payload a real upload would. `verbose` comes from
    /// `CODEREX_CRASH_VERBOSE` and is ignored in strict mode.
    fn telemetry_redaction_modes(&self) -> (bool, bool) {
        let settings_strict = self
            .plugin_host_config_service()
            .resolve()
            .ok()
            .and_then(|settings| settings.telemetry)
            .and_then(|telemetry| telemetry.strict)
            .unwrap_or(false);
        (
            coderex_telemetry::crash_verbose(),
            settings_strict || coderex_telemetry::hipaa_enabled(),
        )
    }

    /// The settings service used to read host-only plugin configuration.
    fn plugin_host_config_service(&self) -> ConfigService {
        let user_path = self
            .ctx
            .settings_path
            .clone()
            .unwrap_or_else(|| coderex_config::config_home().join("settings.json"));
        ConfigService::new(user_path, Self::find_project_config(&self.ctx.cwd))
            .with_local_config_path(Self::local_config_path(&self.ctx.cwd))
    }

    /// Retry failed plugins and actually restart the ones that recover.
    ///
    /// `PluginManager::spawn_retry_loop` only logs `RestartRequired`: it holds
    /// a `Weak` and has no way to await `spawn_plugin` from a synchronous
    /// retry pass. This loop does both — it upgrades the weak reference for
    /// each tick, calls `retry_failed_plugins`, and awaits `spawn_plugin` for
    /// every plugin the retry brought back to `Loading`.
    fn spawn_plugin_restart_loop(
        manager: &Arc<coderex_plugins::plugins::PluginManager>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let manager = Arc::downgrade(manager);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                // Upgrade per tick, then drop the strong reference at the end
                // of the iteration: the task must not keep the manager (and
                // its plugin processes) alive after the session lets go.
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                // A process can die while no command is in flight; move it to
                // Error first so the retry pass can see it.
                let _ = manager.reap_dead_plugins();
                for outcome in manager.retry_failed_plugins() {
                    if let coderex_plugins::plugins::RetryOutcome::RestartRequired(name) = outcome {
                        match manager.spawn_plugin(&name).await {
                            Ok(()) => tracing::info!(
                                plugin = %name,
                                "plugin recovered and was restarted"
                            ),
                            Err(error) => tracing::warn!(
                                plugin = %name,
                                "plugin recovered but restart failed: {error}"
                            ),
                        }
                    }
                }
            }
        })
    }

    /// Subscribe the production sink to every channel the active plugins
    /// declare, returning the shared log it records into.
    ///
    /// Subscribing is what moves a configured channel from `Idle` to
    /// `Subscribed`: a channel with no sink has nowhere for a notification to
    /// go, and `plugin_channel_report()` says so. This is the production
    /// subscriber — the one place the channel layer is actually consumed.
    fn subscribe_plugin_channels(
        manager: &Arc<coderex_plugins::plugins::PluginManager>,
    ) -> Arc<
        std::sync::Mutex<std::collections::VecDeque<coderex_plugins::plugins::PluginChannelEvent>>,
    > {
        let log = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let sink: Arc<dyn coderex_plugins::plugins::ChannelSink> = Arc::new(PluginChannelSink {
            log: Arc::clone(&log),
        });
        for report in manager.plugin_channel_report() {
            manager.subscribe_channel(&report.channel, Arc::clone(&sink));
        }
        log
    }

    /// Marketplaces configured in settings plus the legacy environment pair.
    ///
    /// Later duplicates (by name or URL) are dropped so a settings entry wins
    /// over the environment fallback rather than being registered twice.
    fn configured_marketplaces(
        configured: &[coderex_config::MarketplaceSetting],
    ) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = configured
            .iter()
            .filter_map(|marketplace| {
                let name = marketplace.name.trim();
                let url = marketplace.url.trim();
                (!name.is_empty() && !url.is_empty()).then(|| (name.to_string(), url.to_string()))
            })
            .collect();

        if let Ok(url) = std::env::var("CODEREX_MARKETPLACE_URL") {
            let url = url.trim().to_string();
            if !url.is_empty() {
                let name =
                    std::env::var("CODEREX_MARKETPLACE_NAME").unwrap_or_else(|_| "default".into());
                entries.push((name, url));
            }
        }

        let mut seen_names = std::collections::HashSet::new();
        let mut seen_urls = std::collections::HashSet::new();
        entries.retain(|(name, url)| {
            let new_name = seen_names.insert(name.clone());
            let new_url = seen_urls.insert(url.clone());
            new_name && new_url
        });
        entries
    }

    /// Register configured marketplaces on a plugin manager.
    ///
    /// A registration failure (for example an HTTP URL) is reported and
    /// skipped; it must not stop an install that may still be a direct URL.
    fn register_marketplaces(
        manager: &coderex_plugins::plugins::PluginManager,
        marketplaces: &[(String, String)],
    ) {
        for (name, url) in marketplaces {
            match manager.register_marketplace(name.clone(), url.clone()) {
                Ok(()) => tracing::info!(marketplace = %name, "registered plugin marketplace"),
                Err(error) => {
                    tracing::warn!(marketplace = %name, %url, "marketplace not registered: {error}");
                }
            }
        }
    }

    /// ReconcileMarketplaces: sync marketplace state.
    async fn phase_reconcile_marketplaces(&self) -> Result<()> {
        let manager = plugin_manager_with_signing_key();
        let reconciler = coderex_plugins::plugins::MarketplaceReconciler::with_auto_install_enabled(
            300,
            Self::plugin_auto_install_enabled(),
        );

        let marketplaces = Self::configured_marketplaces(&self.ctx.marketplaces);
        Self::register_marketplaces(&manager, &marketplaces);

        let install_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/plugins");
        match reconciler.reconcile(&manager, Some(&install_dir)).await {
            Ok(report) => {
                if report.is_empty() {
                    tracing::debug!("marketplace reconciliation: no changes");
                } else {
                    tracing::info!(
                        "marketplaces reconciled: {} new, {} updated, {} removed",
                        report.new_count,
                        report.updated_count,
                        report.removed_count
                    );
                }
            }
            Err(e) => {
                tracing::warn!("marketplace reconciliation failed (failing open): {e}");
            }
        }

        Ok(())
    }

    /// InitMcp: initialize MCP server connections (non-blocking, fail-open).
    async fn phase_init_mcp(&mut self) -> Result<()> {
        let settings = self.ctx.settings.clone().unwrap_or_default();
        // Starts with whatever the McpAuth tool persisted, so a server signed
        // into in an earlier session is authenticated in this one.
        let mut manager = coderex_mcp::manager::McpManager::new_with_stored_credentials();

        // A `401 invalid_token` on a server that issued no refresh token is
        // recoverable only by signing in again. The CLI owns the interactive
        // session, so it installs the seam before any server starts: the same
        // McpAuth flow when a person is present, or an authorization-URL
        // hand-off in a headless run. `oauth_mut` is the setup-time hook that
        // does not require the manager to be shared yet.
        manager
            .oauth_mut()
            .set_reauth_handler(Arc::new(crate::mcp_reauth::CliReauthHandler::new(
                !self.ctx.non_interactive,
                self.ctx.cwd.clone(),
            )));
        let manager = Arc::new(manager);

        // Notifications from a configured server must reach the plugin channel
        // layer, which the MCP crate cannot name. Install the router before any
        // server starts so a notification emitted early is not missed.
        if let Some(plugin_manager) = self.ctx.plugin_manager.clone() {
            manager.set_notification_router(Arc::new(PluginChannelRouter {
                manager: plugin_manager,
            }));
        }

        // A server may ask the person a question mid-tool-call. Install the
        // session's answerer before any server is spawned; the TUI attaches
        // its question channel when it starts, and a non-interactive session
        // declines rather than reading a stdin that is not a keyboard.
        let elicitation = SessionElicitationHandler::new(!self.ctx.non_interactive);
        manager.set_inbound_handler(elicitation.inbound());
        self.ctx.mcp_elicitation = Some(elicitation);

        let mut config_loaded = false;

        // Servers contributed by active plugins. Loaded first so the project
        // `.mcp.json` and settings can override a same-named server.
        if let Some(plugin_servers) = self
            .ctx
            .plugin_manager
            .as_ref()
            .and_then(|manager| manager.plugin_mcp_servers())
        {
            match serde_json::from_value::<coderex_mcp::config::McpConfig>(plugin_servers) {
                Ok(config) if !config.is_empty() => {
                    tracing::info!(
                        "MCP config loaded: {} servers from plugins",
                        config.server_names().len()
                    );
                    manager.load_config(&config);
                    config_loaded = true;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("MCP servers from plugins were not loaded: {error}");
                }
            }
        }

        // Servers defined inline in settings are the same shape as
        // `.mcp.json`: either the bare `{name: {...}}` map or wrapped in
        // `{"mcpServers": {...}}`. They are loaded first so the project file
        // below can override a same-named server. This used to only set
        // `config_loaded = true`, which reported MCP as ready while spawning
        // zero servers and warning no one.
        if let Some(ref mcp_servers) = settings.mcp_servers {
            match Self::parse_settings_mcp_servers(mcp_servers) {
                Ok(config) if !config.is_empty() => {
                    tracing::info!(
                        "MCP config loaded: {} servers from settings",
                        config.server_names().len()
                    );
                    manager.load_config(&config);
                    config_loaded = true;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("MCP servers in settings were not loaded: {e}");
                }
            }
        }

        // Load from custom --mcp-config or default .mcp.json
        let mcp_path = self
            .ctx
            .mcp_config_path
            .clone()
            .unwrap_or_else(|| std::path::Path::new(&self.ctx.cwd).join(".mcp.json"));
        if mcp_path.exists() {
            match coderex_mcp::config::McpConfig::load(&mcp_path) {
                Ok(config) => {
                    if !config.is_empty() {
                        manager.load_config(&config);
                        config_loaded = true;
                        tracing::info!(
                            "MCP config loaded: {} servers from {mcp_path:?}",
                            config.server_names().len()
                        );
                    }
                }
                Err(e) => {
                    if self.ctx.strict_mcp_config {
                        return Err(anyhow::anyhow!("strict MCP config validation failed: {e}"));
                    }
                    tracing::warn!("failed to load MCP config from {mcp_path:?}: {e}");
                }
            }
        } else if self.ctx.mcp_config_path.is_some() {
            tracing::warn!("custom MCP config not found: {mcp_path:?}");
        }

        if config_loaded {
            if self.ctx.mcp_debug {
                tracing::info!("MCP debug mode enabled");
            }
            // Spawn all registered MCP servers in the background
            manager.start_all().await;
            let running = manager.list_servers();
            tracing::info!("MCP servers started: {}", running.len());

            // Bridge MCP-discovered tools to skills
            for tool in manager.list_tool_info() {
                let qualified_name = tool.qualified_name();
                self.ctx
                    .mcp_skills
                    .push(coderex_plugins::skills::SkillMetadata {
                        name: qualified_name.clone(),
                        hooks: vec!["post-query".into()],
                        agent: Some(tool.server.clone()),
                        allowed_tools: vec![qualified_name],
                        description: tool.description,
                        enabled: true,
                    });
            }
            if !self.ctx.mcp_skills.is_empty() {
                tracing::info!(
                    "MCP skills bridged: {} tools registered as skills",
                    self.ctx.mcp_skills.len()
                );
            }

            self.ctx.mcp_ready = true;
            // Keep the connections alive: the REPL registers the discovered
            // tools into its registry, and they resolve their connection
            // through this manager at call time.
            self.ctx.mcp_manager = Some(Arc::clone(&manager));
        } else {
            tracing::debug!("no MCP servers configured");
        }

        Ok(())
    }

    /// InitMemory: initialize memory subsystem (auto dream, team memory).
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_init_memory(&mut self) -> Result<()> {
        let memory_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex");

        if !memory_dir.exists() {
            std::fs::create_dir_all(&memory_dir)?;
        }

        tracing::info!("memory subsystem initialized at {memory_dir:?}");
        self.ctx.memory_ready = true;
        Ok(())
    }

    /// InitRemoteServices: initialize remote service connections.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_init_remote_services(&mut self) -> Result<()> {
        // Bridge transport initialization — best-effort, failures don't block startup
        if let Ok(bridge_url) = std::env::var("CODEREX_BRIDGE_URL")
            && !bridge_url.is_empty()
        {
            let factory = coderex_bridge::transport::default_factory();
            tracing::info!(
                "bridge transport initialized: url={bridge_url}, transports={:?}",
                factory.registered_types()
            );
            self.ctx.remote_services_ready = true;
            return Ok(());
        }

        tracing::debug!("no remote services configured");
        self.ctx.remote_services_ready = true;
        Ok(())
    }

    /// Ready: final verification that all critical subsystems are up.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn phase_ready(&self) -> Result<()> {
        tracing::info!(
            "startup complete: cwd={}, model={}, mcp={}, memory={}, remote={}",
            self.ctx.cwd,
            self.ctx.model.as_ref().map_or("default", |m| m.0.as_str()),
            self.ctx.mcp_ready,
            self.ctx.memory_ready,
            self.ctx.remote_services_ready,
        );
        Ok(())
    }

    /// The project-local settings file read after the project file.
    ///
    /// `/init` writes this path. It is not searched in ancestors: local
    /// settings belong to the directory the session was started in.
    fn local_config_path(cwd: &str) -> std::path::PathBuf {
        std::path::Path::new(cwd).join(".coderex/settings.local.json")
    }

    /// Find project config in CWD or nearest ancestor.
    fn find_project_config(cwd: &str) -> Option<std::path::PathBuf> {
        let mut current = std::path::Path::new(cwd);
        loop {
            let candidate = current.join(".coderex/settings.json");
            if candidate.exists() {
                return Some(candidate);
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        None
    }

    /// Accept `--effort low|medium|high`, rejecting anything else.
    ///
    /// The level is carried into every request context. A provider with no
    /// reasoning-effort control omits it and warns, so this checks the spelling
    /// of the request rather than promising that every provider honours it.
    fn validate_effort(effort: Option<&str>) -> Result<()> {
        if let Some(level) = effort
            && ReasoningEffort::parse(level).is_none()
        {
            bail!("invalid --effort '{level}': expected one of low, medium, high");
        }
        Ok(())
    }

    /// Wrap an API client so the chosen reasoning effort rides every request.
    ///
    /// `validate_effort` has already rejected anything but low/medium/high, so
    /// an unparsable value cannot reach here; `None` covers the unset flag.
    fn apply_effort(client: Box<dyn ApiClient>, effort: Option<&str>) -> Box<dyn ApiClient> {
        match effort.and_then(ReasoningEffort::parse) {
            Some(effort) => Box::new(EffortClient::new(client, effort)),
            None => client,
        }
    }

    /// Create an API client from environment variables.
    ///
    /// Uses `ProviderConfig::from_env()` to build a [`ModelRouter`] that
    /// supports multi-provider fallback (Anthropic → OpenAI → Groq → custom).
    fn try_create_api_client(
        api_key_override: Option<&str>,
        prompt_caching: Option<bool>,
        provider: Option<&coderex_config::ProviderSettings>,
    ) -> Option<Box<dyn ApiClient>> {
        Self::api_client_from_provider_config(
            ProviderConfig::from_env(),
            api_key_override,
            prompt_caching,
            provider,
            |name| std::env::var(name).ok(),
        )
    }

    /// Build an API client from a [`ProviderConfig`], gated on usable
    /// credentials.
    ///
    /// Split out from [`Self::try_create_api_client`] so the gate can be
    /// tested without mutating process environment variables. A cloud
    /// deployment is a provider in its own right: it must not require a
    /// first-party Anthropic key when the configured cloud auth style supplies
    /// credentials (for example AWS SigV4 variables for Bedrock).
    fn api_client_from_provider_config(
        mut config: ProviderConfig,
        api_key_override: Option<&str>,
        prompt_caching: Option<bool>,
        provider: Option<&coderex_config::ProviderSettings>,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> Option<Box<dyn ApiClient>> {
        if let Some(key) = api_key_override {
            config.set_anthropic_api_key(key.into());
        }
        // Settings-level endpoint overrides. The base URL is applied after the
        // key so it reaches the endpoint the key created; the project layers
        // are stripped from this block during settings resolution.
        if let Some(provider) = provider {
            if let Some(ref base_url) = provider.base_url {
                config.set_anthropic_base_url(base_url.clone());
            }
            config.request_timeout_secs = provider.request_timeout_secs;
        }
        // The resolved setting rides into the provider config; the core
        // builder combines it with the environment override.
        config.prompt_caching = prompt_caching;
        // Core's transport rejects an empty key before it reaches the cloud
        // auth branch. For AWS SigV4 providers the key is not actually used
        // when AWS credentials are present, so a non-empty sentinel lets the
        // request reach the signer. Other cloud kinds genuinely need a token.
        if let Some(kind) = config.cloud.as_ref().map(|cloud| cloud.kind) {
            let has_key = config
                .anthropic
                .as_ref()
                .is_some_and(|endpoint| !endpoint.api_key.trim().is_empty());
            let aws_credentials = Self::env_non_empty(&get_env, "AWS_ACCESS_KEY_ID")
                && Self::env_non_empty(&get_env, "AWS_SECRET_ACCESS_KEY");
            let uses_sigv4 = matches!(
                kind,
                ProviderKind::Bedrock | ProviderKind::AnthropicAws | ProviderKind::Mantle
            );
            if uses_sigv4 && !has_key && aws_credentials {
                config.set_anthropic_api_key("aws-sigv4".into());
            }
        }
        if !Self::provider_config_is_usable_with(&config, &get_env) {
            return None;
        }
        Some(Box::new(config.build_router()) as Box<dyn ApiClient>)
    }

    fn env_non_empty(get_env: &impl Fn(&str) -> Option<String>, name: &str) -> bool {
        get_env(name).is_some_and(|value| !value.trim().is_empty())
    }

    /// Whether a provider configuration has credentials the transport can use.
    fn provider_config_is_usable_with(
        config: &ProviderConfig,
        get_env: &impl Fn(&str) -> Option<String>,
    ) -> bool {
        let non_empty = |value: &str| !value.trim().is_empty();
        if config
            .anthropic
            .as_ref()
            .is_some_and(|endpoint| non_empty(&endpoint.api_key))
            || config
                .openai
                .as_ref()
                .is_some_and(|endpoint| non_empty(&endpoint.api_key))
            || config
                .groq
                .as_ref()
                .is_some_and(|endpoint| non_empty(&endpoint.api_key))
            || config
                .custom
                .iter()
                .any(|endpoint| non_empty(&endpoint.api_key))
        {
            return true;
        }
        config
            .cloud
            .as_ref()
            .is_some_and(|cloud| Self::cloud_credentials_present_with(cloud, get_env))
    }

    /// Whether the credentials the cloud transport reads are present.
    ///
    /// AWS-backed providers can authenticate with SigV4 (`AWS_ACCESS_KEY_ID` /
    /// `AWS_SECRET_ACCESS_KEY`) or with a bearer key. Vertex, Anthropic on
    /// Google Cloud, and Foundry use the API key slot as their token, because
    /// that is what the core transport sends. A gateway may be unauthenticated.
    fn cloud_credentials_present_with(
        cloud: &CloudEndpoint,
        get_env: &impl Fn(&str) -> Option<String>,
    ) -> bool {
        let present = |name: &str| get_env(name).is_some_and(|value| !value.trim().is_empty());
        let api_key = present("CODEREX_API_KEY")
            || present("CODEREX_AUTH_TOKEN")
            || present("ANTHROPIC_API_KEY")
            || present("ANTHROPIC_AUTH_TOKEN");
        match cloud.kind {
            ProviderKind::Bedrock | ProviderKind::AnthropicAws | ProviderKind::Mantle => {
                api_key || (present("AWS_ACCESS_KEY_ID") && present("AWS_SECRET_ACCESS_KEY"))
            }
            ProviderKind::Vertex | ProviderKind::AnthropicGoogleCloud | ProviderKind::Foundry => {
                api_key
            }
            ProviderKind::Gateway | ProviderKind::FirstParty => true,
        }
    }

    /// Get the API key override from settings if set.
    fn api_key_from_settings(&self) -> Option<&str> {
        self.ctx
            .settings
            .as_ref()
            .and_then(|s| s.api_key.as_deref())
    }

    /// Provider endpoint overrides from settings (user/managed layers only).
    fn provider_settings(&self) -> Option<&coderex_config::ProviderSettings> {
        self.ctx
            .settings
            .as_ref()
            .and_then(|settings| settings.provider.as_ref())
    }

    /// The resolved `prompt_caching` setting, if the user set one.
    ///
    /// `CODEREX_PROMPT_CACHING` is applied by the core client on top of this.
    fn prompt_caching_from_settings(&self) -> Option<bool> {
        self.ctx
            .settings
            .as_ref()
            .and_then(|settings| settings.prompt_caching)
    }

    /// Apply the user `env` block to this process.
    ///
    /// Settings resolution strips `env` from the project and project-local
    /// layers, so only the user and a checksum-verified managed file can reach
    /// this method. The block is applied once at startup, before the model
    /// resolver, provider construction, feature flags and telemetry channels
    /// read the environment.
    #[allow(unsafe_code)] // `set_var` is unsafe in edition 2024; applied once at startup.
    fn apply_settings_env(settings: &coderex_config::Settings) {
        let Some(env) = settings.env.as_ref() else {
            return;
        };
        for (key, value) in env {
            // SAFETY: edition 2024 makes `set_var` unsafe because a concurrent
            // reader could observe the mutation. Startup applies the block in
            // one place, before the readers listed above run; the threads that
            // already exist (the runtime's I/O drivers and the plugin monitor
            // supervisor) do not consult these keys. This is the same trade-off
            // a Claude-compatible `env` block makes.
            unsafe { std::env::set_var(key, value) };
        }
    }

    /// Configure a ReplLoop with all CLI-derived settings.
    ///
    /// `interactive` selects context delivery: headless builds the whole
    /// prompt itself (including `--file` blocks), while the interactive loop
    /// has no public file hook yet and receives the same blocks through the
    /// existing extra-context channel that `--from-pr` uses.
    fn configure_repl(&self, repl: &mut crate::repl::ReplLoop, interactive: bool) {
        if let Some(ref manager) = self.ctx.mcp_manager {
            repl.with_mcp_manager(Arc::clone(manager));
        }
        if let Some(ref elicitation) = self.ctx.mcp_elicitation {
            repl.with_mcp_elicitation_handler(Arc::clone(elicitation));
        }
        repl.with_skip_permissions(self.ctx.skip_permissions);
        // Only the headless path reads `control_response` from stdin; the
        // interactive loop has its own confirmation UI and owns stdin for the
        // editor.
        if !interactive {
            repl.with_headless_permission_control(self.ctx.headless_permission_control);
        }
        let perm_mode = self
            .ctx
            .settings
            .as_ref()
            .and_then(|s| s.permission_mode.as_deref())
            .and_then(|m| m.parse::<coderex_core::permissions::PermissionMode>().ok());
        repl.with_permission_mode(perm_mode);
        repl.with_permission_rules(
            self.ctx
                .settings
                .as_ref()
                .and_then(|settings| settings.permissions.clone()),
        );
        repl.with_no_persistence(self.ctx.no_persistence);
        repl.with_no_slash_commands(self.ctx.no_slash_commands);
        repl.with_json_schema(self.ctx.json_schema.clone());
        let budget = self.ctx.settings.as_ref().and_then(|s| s.max_budget_usd);
        repl.with_max_budget_usd(budget);
        let hooks = self.ctx.settings.as_ref().and_then(|s| s.hooks.clone());
        repl.with_hooks(hooks);
        // The merged document above is only the compatibility fallback. The
        // split view is what preserves the managed-before-core guarantee.
        let mut hook_layers = self
            .ctx
            .settings
            .as_ref()
            .and_then(|s| s.hook_layers.clone());
        // Plugins are the producer for the `Plugin` tier: their manifests
        // contribute hooks that run ahead of core hooks and behind managed
        // ones. Without this the tier could only ever be empty.
        if let Some(plugin_hooks) = self
            .ctx
            .plugin_manager
            .as_ref()
            .and_then(|manager| manager.plugin_hooks())
        {
            let layers = hook_layers.get_or_insert_with(|| coderex_config::settings::HookLayers {
                core: None,
                plugin: None,
                managed: None,
            });
            layers.plugin = Some(plugin_hooks);
        }
        repl.with_hook_layers(hook_layers);
        repl.with_max_turns(self.ctx.max_turns);
        repl.with_fallback_model(self.ctx.fallback_model.clone());
        repl.with_telemetry_handle(self.ctx.telemetry_handle.clone());
        // `/vim` and `/statusline-setup` persist these; apply them at startup.
        let vim_mode = self
            .ctx
            .settings
            .as_ref()
            .and_then(|settings| settings.vim_mode);
        let statusline = self
            .ctx
            .settings
            .as_ref()
            .and_then(|settings| settings.statusline.as_ref());
        let statusline_theme = statusline.and_then(|statusline| statusline.theme.clone());
        let statusline_format = statusline.and_then(|statusline| statusline.format.clone());
        repl.with_startup_preferences(vim_mode, statusline_theme, statusline_format);
        repl.with_replay_user_messages(self.ctx.replay_user_messages);
        repl.with_include_partial_messages(self.ctx.include_partial_messages);
        repl.with_betas(self.ctx.betas.clone());
        // `--system-prompt` replaces the base prompt; `--append-system-prompt`
        // adds to it. They are independent: when both are given, the
        // replacement stays authoritative for the base and the append text is
        // applied as an extra instruction on top of it. The headless path
        // folds both into the user prompt (`build_headless_prompt`), so
        // applying them here as well would send them twice.
        if interactive {
            repl.with_system_prompt(self.ctx.system_prompt.clone());
            repl.with_append_system_prompt(self.ctx.append_system_prompt.clone());
        }
        repl.with_chrome_enabled(self.ctx.chrome && !self.ctx.no_chrome);
        repl.with_agent(self.ctx.agent.clone());
        if let Some(ref manager) = self.ctx.plugin_manager {
            repl.with_plugin_agents(manager.plugin_agents());
            self.configure_plugin_components(repl, manager);
        }
        repl.with_explicit_model_override(self.ctx.model_explicit);
        repl.with_worktree(self.ctx.worktree);
        repl.with_fork_session(self.ctx.fork_session.clone());
        repl.with_from_pr(self.ctx.from_pr.clone());
        repl.with_pr_context(self.interactive_context(interactive));
        repl.with_session_name(self.ctx.session_name.clone());
        repl.with_add_dir(self.ctx.add_dir.clone());
        repl.with_input_format(self.ctx.input_format.clone());
        repl.with_tmux_enabled(self.ctx.tmux);
        repl.with_debug_file(self.ctx.debug_file.clone());
        if !self.ctx.allowed_tools.is_empty() {
            repl.with_allowed_tools(self.ctx.allowed_tools.clone());
        }
        if let Some(settings) = &self.ctx.settings {
            repl.with_context_window_settings(settings.context_window_overrides());
        }
        repl.apply_cli_config();
    }

    /// Hand the REPL the components active plugins contribute.
    ///
    /// The plugin crate normalizes each manifest shape into a name-keyed map;
    /// this turns those maps into the forms the session consumes and keeps a
    /// flat list of the remaining components so `/plugin` can show them.
    fn configure_plugin_components(
        &self,
        repl: &mut crate::repl::ReplLoop,
        manager: &Arc<coderex_plugins::plugins::PluginManager>,
    ) {
        let entries = |value: Option<serde_json::Value>| -> Vec<(String, serde_json::Value)> {
            value
                .and_then(|value| value.as_object().cloned())
                .map(|object| object.into_iter().collect())
                .unwrap_or_default()
        };
        let themes = entries(manager.plugin_themes());
        let output_styles = entries(manager.plugin_output_styles());
        let lsp_servers = entries(manager.plugin_lsp_servers());
        let workflows: std::collections::HashMap<String, String> =
            entries(manager.plugin_workflows())
                .into_iter()
                .filter_map(|(name, value)| {
                    let script = match value {
                        serde_json::Value::String(script) => script,
                        serde_json::Value::Object(mut object) => object
                            .remove("script")
                            .and_then(|script| script.as_str().map(str::to_string))
                            .or_else(|| {
                                object
                                    .get("path")
                                    .and_then(|path| path.as_str())
                                    .and_then(|path| std::fs::read_to_string(path).ok())
                            })?,
                        _ => return None,
                    };
                    Some((name, script))
                })
                .collect();
        let monitors = manager.plugin_monitors();
        let mut other = Vec::new();
        // A channel names an MCP server. The report makes that reference real:
        // each channel is listed with its live state — subscribed, idle, or a
        // server that is not configured — so `/plugin components` shows the
        // truth about whether a notification could actually arrive.
        for report in manager.plugin_channel_report() {
            if report.status == coderex_plugins::plugins::ChannelStatus::ServerNotConfigured {
                tracing::warn!(
                    channel = %report.channel,
                    server = %report.server,
                    "plugin channel names an MCP server that is not configured"
                );
            }
            other.push(("channel".to_string(), vec![report.render()]));
        }
        if let Some(dir) = self.ctx.plugin_types_dir.clone() {
            other.push(("types-dir".to_string(), vec![dir]));
        }
        // The registry is the runtime consumer for the `types` field: the
        // `.d.ts` files are read into memory (never executed) and listed here,
        // each named after its plugin so a reader can tell them apart.
        let type_registry = manager.plugin_type_registry();
        if !type_registry.is_empty() {
            other.push((
                "types".to_string(),
                type_registry
                    .list()
                    .iter()
                    .map(|file| format!("{} ({})", file.name, file.plugin))
                    .collect(),
            ));
        }
        for (plugin, error) in type_registry.errors() {
            tracing::warn!(plugin = %plugin, %error, "plugin types were not loaded");
        }
        for (kind, value) in [
            ("binaries", manager.plugin_binaries()),
            ("experimental", manager.plugin_experimental()),
        ] {
            if let Some(object) = value.and_then(|value| value.as_object().cloned()) {
                other.push((
                    kind.to_string(),
                    object.into_iter().map(|(key, _)| key).collect(),
                ));
            }
        }
        repl.with_plugin_components(
            themes,
            output_styles,
            workflows,
            monitors,
            lsp_servers,
            other,
        );
    }

    /// Context delivered to the interactive loop's extra-context channel.
    ///
    /// Headless sessions get files from [`Self::build_headless_prompt`]; the
    /// interactive path has no dedicated file hook, so it reuses the same
    /// extra-context mechanism as `--from-pr`. System prompts are not included
    /// here: they belong in the engine's system prompt, not a user message.
    fn interactive_context(&self, interactive: bool) -> Option<String> {
        let mut context = self.ctx.from_pr_context.clone();
        if !interactive {
            return context;
        }
        for file_path in &self.ctx.file {
            match std::fs::read_to_string(file_path) {
                Ok(content) => {
                    let block = format!("--- {} ---\n{content}", file_path.display());
                    match &mut context {
                        Some(existing) => {
                            existing.push_str("\n\n");
                            existing.push_str(&block);
                        }
                        None => context = Some(block),
                    }
                }
                Err(error) => {
                    tracing::warn!("failed to read file {}: {error}", file_path.display());
                }
            }
        }
        context
    }

    /// Offer the session's model to MCP servers when sampling was opted into.
    ///
    /// The manager already has the elicitation handler installed; this wraps
    /// it so `sampling/createMessage` reaches the CLI handler and every other
    /// method still reaches the elicitation handler. Without the opt-in,
    /// nothing changes and sampling stays refused by default.
    fn install_sampling_handler(&self, client: &dyn ApiClient, model: &ModelId) {
        let Some(manager) = &self.ctx.mcp_manager else {
            return;
        };
        let Some(sampling) =
            crate::mcp_sampling::handler_from_env(client.clone_box(), model.clone())
        else {
            return;
        };
        let other = self
            .ctx
            .mcp_elicitation
            .as_ref()
            .map(SessionElicitationHandler::inbound);
        manager.set_inbound_handler(Arc::new(coderex_mcp::elicitation::McpRequestRouter::new(
            Some(sampling),
            other,
        )));
        tracing::info!(
            model = %model.0,
            "MCP sampling enabled by {}",
            coderex_mcp::elicitation::SAMPLING_OPT_IN_ENV
        );
    }

    /// Build and configure a REPL loop for a model.
    ///
    /// The one-retry `--fallback-model` behaviour lives in the engine, so this
    /// no longer needs to build a second loop after a failure.
    fn prepare_repl(
        &self,
        cwd: &str,
        model: ModelId,
        streaming_protocol: bool,
        interactive: bool,
    ) -> crate::repl::ReplLoop {
        let api_key = self.api_key_from_settings();
        let prompt_caching = self.prompt_caching_from_settings();
        let mut repl = if let Some(api_client) =
            Self::try_create_api_client(api_key, prompt_caching, self.provider_settings())
        {
            let api_client = Self::apply_effort(api_client, self.ctx.effort.as_deref());
            self.install_sampling_handler(api_client.as_ref(), &model);
            crate::repl::ReplLoop::with_engine(api_client, cwd.to_string(), model)
        } else {
            crate::repl::ReplLoop::new(cwd.to_string(), model)
        };
        if let Some(ref pm) = self.ctx.plugin_manager {
            repl.with_plugin_manager(Arc::clone(pm));
        }
        self.configure_repl(&mut repl, interactive);
        if streaming_protocol {
            repl.with_protocol_sink(Some(coderex_events::MessageSink::new(
                Box::new(std::io::stdout()) as Box<dyn std::io::Write + Send + Sync>,
            )));
            // The host asked for the protocol, so it can answer tool approvals:
            // an `Ask` becomes a `control_request` frame and the answer is read
            // from stdin as a `control_response`.
            repl.with_headless_permission_control(true);
        }
        repl
    }

    // The future borrows the non-`Sync` REPL loop, so it is not `Send`; it is
    // awaited from `run` on the current task and never spawned.
    #[allow(clippy::future_not_send)]
    #[allow(clippy::unused_async)]
    async fn run_repl(&self, cwd: &str, model: ModelId) -> Result<()> {
        if !std::io::stdin().is_terminal() {
            bail!("interactive REPL requires a terminal on stdin");
        }

        let mut repl = self.prepare_repl(cwd, model, false, true);
        repl.run_tui().await
    }

    // The future borrows the non-`Sync` REPL loop, so it is not `Send`; it is
    // awaited from `run` on the current task and never spawned.
    #[allow(clippy::future_not_send)]
    #[allow(clippy::unused_async)]
    async fn run_headless_with_prompt(
        &self,
        cwd: &str,
        model: ModelId,
        prompt: &str,
    ) -> Result<()> {
        // Parse input based on --input-format
        let user_prompt = if self.ctx.input_format == "json" {
            serde_json::from_str::<serde_json::Value>(prompt).map_or_else(
                |_| prompt.to_string(),
                |val| {
                    val.get("prompt")
                        .and_then(|p| p.as_str())
                        .unwrap_or(prompt)
                        .to_string()
                },
            )
        } else {
            prompt.to_string()
        };

        let final_prompt = self.build_headless_prompt(&user_prompt);

        // `stream-json` is a protocol, not a formatted answer: the turn is
        // written to stdout as it happens, one JSON object per line, and the
        // host reads that instead of a printed string. So the sink goes on the
        // loop before the turn starts, and nothing is echoed after it.
        let streaming_protocol = self.ctx.output_format == "stream-json";
        let mut repl = self.prepare_repl(cwd, model.clone(), streaming_protocol, false);

        // The engine owns the one-retry fallback for both interactive and
        // headless sessions, so the headless path must not retry again: doing
        // so would send the same failure to the same fallback a second time.
        let result = repl.run_headless(&final_prompt).await?;

        if streaming_protocol {
            return Ok(());
        }

        // Apply output formatting
        let formatted = self.format_headless_output(&result);
        println!("{formatted}");
        Ok(())
    }

    /// Assemble the non-interactive prompt.
    ///
    /// Order matters: the pull-request block comes first so it is part of the
    /// prompt before any instruction the user typed, then the system prompt,
    /// included files, and finally the user's own words.
    fn build_headless_prompt(&self, user_prompt: &str) -> String {
        use std::fmt::Write as _;

        let mut final_prompt = String::new();

        if let Some(ref context) = self.ctx.from_pr_context {
            final_prompt.push_str(context);
            final_prompt.push_str("\n\n");
        }

        // Headless turns carry the prompt flags in the user prompt instead of
        // the engine's system prompt (the interactive path is the one that
        // sets `SessionEffects`). The two flags are independent and may both
        // appear: `--system-prompt` supplies the replacement base and
        // `--append-system-prompt` is labelled as additive on top of it.
        if let Some(ref sp) = self.ctx.system_prompt {
            final_prompt.push_str("System: ");
            final_prompt.push_str(sp);
            final_prompt.push('\n');
            final_prompt.push('\n');
        }

        if let Some(ref append) = self.ctx.append_system_prompt {
            final_prompt.push_str("Additional system instructions: ");
            final_prompt.push_str(append);
            final_prompt.push('\n');
            final_prompt.push('\n');
        }

        for file_path in &self.ctx.file {
            match std::fs::read_to_string(file_path) {
                Ok(content) => {
                    let _ = write!(
                        final_prompt,
                        "--- {} ---\n{}\n\n",
                        file_path.display(),
                        content
                    );
                }
                Err(e) => {
                    tracing::warn!("failed to read file {}: {e}", file_path.display());
                }
            }
        }

        final_prompt.push_str(user_prompt);
        final_prompt
    }

    /// Format headless output based on --output-format, --brief, --bare flags.
    fn format_headless_output(&self, raw: &str) -> String {
        let mut output = raw.to_string();

        if self.ctx.bare || self.ctx.brief {
            // Strip markdown decorations and extra whitespace
            output = output.trim().to_string();
        }

        if self.ctx.output_format == "json" {
            let json_obj = serde_json::json!({
                "response": output,
                "model": self.ctx.model.as_ref().map(|m| m.0.clone()),
            });
            return json_obj.to_string();
        }

        if self.ctx.output_format == "stream-json" {
            // The protocol has already been written by the time this runs; the
            // caller returns before reaching here. Reaching it would mean the
            // sink was never installed, and printing the answer as text on top
            // of a half-written stream would corrupt it.
            tracing::warn!(
                "stream-json was requested but no protocol sink was installed; no output was written"
            );
            return String::new();
        }

        output
    }

    #[allow(clippy::unused_async)]
    async fn run_server(&self, cwd: &str, port: u16) -> Result<()> {
        use coderex_server::{DaemonServer, ServerConfig};

        // Write PID file before starting
        daemon::write_pid_file().map_err(|e| anyhow::anyhow!("Failed to write PID file: {e}"))?;

        // Build server config from environment
        let valid_keys: Vec<String> = std::env::var("CODEREX_API_KEYS")
            .ok()
            .map(|s| s.split(',').map(String::from).collect())
            .unwrap_or_default();

        let idle_timeout = std::env::var("CODEREX_IDLE_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3600);

        // Session persistence used to be hard-coded to `None`, which made the
        // whole store unreachable from the production binary: sessions did not
        // survive a restart, and the "advance the id counter past restored
        // sessions" fix never ran. Persist to the config directory by default;
        // `CODEREX_SERVER_PERSIST_PATH` overrides the file, and
        // `CODEREX_SERVER_NO_PERSIST=1` turns it off. A failure to open the
        // store is already non-fatal (the server warns and runs in memory).
        let persist_path = if Self::env_flag("CODEREX_SERVER_NO_PERSIST") {
            None
        } else {
            Some(std::env::var_os("CODEREX_SERVER_PERSIST_PATH").map_or_else(
                || coderex_config::config_home().join("server-sessions.db"),
                std::path::PathBuf::from,
            ))
        };
        if let Some(ref path) = persist_path {
            tracing::info!("server session store: {}", path.display());
        }

        let permission_mode = if self.ctx.skip_permissions {
            coderex_core::permissions::PermissionMode::BypassPermissions
        } else {
            self.ctx
                .settings
                .as_ref()
                .and_then(|settings| settings.permission_mode.as_deref())
                .and_then(|mode| mode.parse().ok())
                .unwrap_or_default()
        };

        let config = ServerConfig {
            valid_keys,
            allow_anonymous: false,
            idle_timeout: std::time::Duration::from_secs(idle_timeout),
            max_sessions: 100,
            rate_limit_per_minute: 60,
            health_port: None,
            persist_path,
        };

        let mut server = DaemonServer::with_config(config);

        // Serve queries on a real engine. Without this the server could only
        // refuse them — and before it did, it echoed them back, which is worse
        // than refusing because it looked like an answer.
        let context_window = coderex_core::types::ContextWindowResolver::from_env_and_client_data()
            .with_settings(
                self.ctx
                    .settings
                    .as_ref()
                    .map(coderex_config::Settings::context_window_overrides)
                    .unwrap_or_default(),
            )
            .with_experiment(
                coderex_config::flags::FeatureFlags::from_env().experiment_context_windows(),
            );
        if let Some(factory) = Self::serve_engine_factory(
            cwd,
            Self::resolve_session_model(
                self.ctx.model.clone(),
                coderex_config::flags::FeatureFlags::from_env().fast_mode,
            ),
            context_window,
            self.ctx.mcp_manager.clone(),
            permission_mode,
            self.ctx
                .settings
                .as_ref()
                .and_then(|settings| settings.permissions.clone()),
            self.ctx.effort.as_deref(),
            self.prompt_caching_from_settings(),
            self.api_key_from_settings(),
            self.provider_settings(),
        ) {
            server.set_engine_factory(factory);
        } else {
            tracing::warn!(
                "no API key configured: served sessions will refuse queries with 'no_engine'"
            );
        }

        tracing::info!("CodeRex server initialized: cwd={cwd}");
        tracing::info!(
            "Server config: idle_timeout={idle_timeout}s, max_sessions=100, port={port}"
        );
        println!("CodeRex server running on ws://0.0.0.0:{port}");
        println!("Sessions: 0 active");
        println!("Press Ctrl+C to stop");

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        let result = server.run(addr).await.map_err(|e| anyhow::anyhow!(e));

        // Clean up PID file on shutdown
        let _ = daemon::remove_pid_file();

        result
    }

    /// Whether a boolean-ish environment variable is set to true.
    fn env_flag(name: &str) -> bool {
        std::env::var(name)
            .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    }

    /// Choose the session model from an explicitly configured one and
    /// `fast_mode`.
    ///
    /// Precedence: an explicit model always wins — `--model`,
    /// `CODEREX_MODEL`/`ANTHROPIC_MODEL`/`OPENAI_MODEL`, or a settings `model`
    /// value. `fast_mode`
    /// only changes the default used when nothing else chose one: on, the
    /// session resolves through [`ModelProfile::Lightweight`] (haiku,
    /// overridable via `CODEREX_LIGHTWEIGHT_MODEL`); off, it is
    /// [`ModelId::sonnet`].
    fn resolve_session_model(explicit: Option<ModelId>, fast_mode: bool) -> ModelId {
        explicit.unwrap_or_else(|| {
            if fast_mode {
                ModelProfile::Lightweight.resolve_runtime()
            } else {
                ModelId::sonnet()
            }
        })
    }

    /// Whether marketplace reconciliation may install new plugins.
    ///
    /// Off by default: installing third-party code automatically is a
    /// decision the user makes, not a side effect of opening the session.
    fn plugin_auto_install_enabled() -> bool {
        Self::env_flag("CODEREX_PLUGIN_AUTO_INSTALL")
    }

    /// Parse the `mcp_servers` settings value.
    ///
    /// Accepts both spellings the ecosystem uses: the bare
    /// `{name: {...}}` map and the wrapped `{"mcpServers": {...}}` object.
    /// `McpConfig` ignores unknown fields, so a bare map parses as an empty
    /// config without an error; the wrapped retry is what makes it usable.
    fn parse_settings_mcp_servers(
        value: &serde_json::Value,
    ) -> Result<coderex_mcp::config::McpConfig, String> {
        if value.is_null() {
            return Ok(coderex_mcp::config::McpConfig::new());
        }
        if let Ok(config) = serde_json::from_value::<coderex_mcp::config::McpConfig>(value.clone())
            && !config.is_empty()
        {
            return Ok(config);
        }
        let wrapped = serde_json::json!({ "mcpServers": value });
        serde_json::from_value::<coderex_mcp::config::McpConfig>(wrapped)
            .map_err(|error| error.to_string())
    }

    /// The engine every served session runs its queries on.
    ///
    /// One per session, built from the same pieces the REPL uses: the routed
    /// API client, the session's model, and the standard tool set behind the
    /// registry dispatcher. Returns `None` when no provider is configured —
    /// the server then says so rather than answering with something it did not
    /// ask a model.
    fn serve_engine_factory(
        cwd: &str,
        model: ModelId,
        context_window: coderex_core::types::ContextWindowResolver,
        mcp_manager: Option<std::sync::Arc<coderex_mcp::manager::McpManager>>,
        permission_mode: coderex_core::permissions::PermissionMode,
        permissions: Option<coderex_config::PermissionsSettings>,
        effort: Option<&str>,
        prompt_caching: Option<bool>,
        api_key: Option<&str>,
        provider: Option<&coderex_config::ProviderSettings>,
    ) -> Option<coderex_server::session::EngineFactory> {
        use coderex_tools::{
            ChromeClient, CronScheduler, ExecutionConfig, ExecutionContext, RateLimiter,
            RegistryToolDispatcher, TaskStore, ToolExecutor as ToolsExecutor, ToolRegistry,
            register_builtin_tools,
        };

        let env_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let api_client =
            Self::try_create_api_client(api_key.or(env_key.as_deref()), prompt_caching, provider)?;
        let api_client = Self::apply_effort(api_client, effort);

        let cwd = cwd.to_string();
        // One manager slot shared by every served session; the manager is the
        // same object the startup phases populated.
        let mcp_slot: crate::repl::McpManagerSlot =
            std::sync::Arc::new(std::sync::RwLock::new(mcp_manager));
        Some(std::sync::Arc::new(move || {
            let mut registry = ToolRegistry::new();
            register_builtin_tools(&mut registry);
            // Same wired baseline tools the REPL registers.
            crate::repl::RuntimeToolHandles::new(&cwd, &mcp_slot).install(&mut registry);

            let config_home = coderex_config::config_home();
            let session_id = uuid::Uuid::new_v4().to_string();

            // The same shared state the REPL installs. Without it a served
            // session's background tools, cron jobs, and rate-limited web
            // tools each behaved as if no session state existed.
            let task_store =
                std::sync::Arc::new(TaskStore::new().with_storage(config_home.join("tasks.jsonl")));
            let _ = task_store.load();
            let cron_scheduler = std::sync::Arc::new(
                CronScheduler::new()
                    .with_storage(config_home.join("scheduled_tasks.json"))
                    .with_job_records(std::path::PathBuf::from(&cwd), session_id.clone()),
            );
            let _ = cron_scheduler.load();

            let registry = std::sync::Arc::new(registry);
            let mut dispatcher = RegistryToolDispatcher::new(
                std::sync::Arc::clone(&registry),
                std::sync::Arc::new(ToolsExecutor::new(ExecutionConfig::default())),
                ExecutionContext::Normal,
                std::path::PathBuf::from(&cwd),
            );
            let flags = coderex_config::flags::FeatureFlags::from_env();
            dispatcher.set_feature_flags(flags.clone());
            dispatcher.set_task_store(task_store);
            dispatcher.set_cron_scheduler(cron_scheduler);
            dispatcher.set_rate_limiter(std::sync::Arc::new(RateLimiter::new()));
            dispatcher.set_chrome_client(ChromeClient::new());
            dispatcher.set_session_id(session_id);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            // A served session used to run on `Default` with no hooks, so a
            // configured `auto`/`bypass-permissions` mode and the classifier,
            // explanation and background-deny hooks were all ignored on this
            // path while they were active in the REPL.
            let mut checker = coderex_core::permissions::PermissionChecker::new(permission_mode);
            if let Err(error) = coderex_config::permission_rules::install_settings_rules(
                &mut checker,
                permissions.as_ref(),
                std::path::Path::new(&cwd),
                None,
            ) {
                tracing::error!(%error, "served session permission rules were not installed");
            }
            let mut permission_engine =
                coderex_core::permission_engine::PermissionEngine::new(checker);
            permission_engine.add_hook(std::sync::Arc::new(
                coderex_core::permission_engine::ExplainPermissionHook::new_with_event_tx(
                    api_client.clone_box(),
                    tx.clone(),
                ),
            ));
            permission_engine.add_hook(std::sync::Arc::new(
                coderex_core::permission_engine::AutoClassifierHook::new(api_client.clone_box()),
            ));
            permission_engine.add_hook(std::sync::Arc::new(
                coderex_core::permission_engine::BackgroundAgentDenyHook,
            ));
            coderex_tools::install_rule_exemptions(&mut permission_engine, &registry);

            let mut engine = coderex_core::engine::QueryEngine::new(
                api_client.clone_box(),
                model.clone(),
                coderex_core::state::SessionState::new(std::path::PathBuf::from(&cwd)),
                permission_engine,
                tx,
            );
            engine.set_tool_executor(Box::new(dispatcher));
            engine.set_context_window_resolver(context_window.clone());
            // The same flag behaviours the REPL installs: a served session
            // honours `coordinator_mode` and `history_snip` too.
            let memory_base = std::path::PathBuf::from(&cwd)
                .join(".coderex")
                .join("memory");
            crate::repl::apply_engine_flag_behaviours(&mut engine, &flags, Some(&memory_base));
            (engine, rx)
        }))
    }

    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn run_serve_stop(&self) -> Result<()> {
        match daemon::stop_daemon() {
            Ok(msg) => {
                println!("{msg}");
                Ok(())
            }
            Err(e) => bail!("{e}"),
        }
    }

    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn run_serve_status(&self) -> Result<()> {
        println!("{}", daemon::serve_status());
        Ok(())
    }

    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn run_agents(&self, detailed: bool) -> Result<()> {
        println!("Built-in agents:");
        if detailed {
            println!("  general_purpose   - General multi-step tasks");
            println!("  explore           - Codebase exploration (quick/medium/very_thorough)");
            println!("  plan              - Implementation planning");
            println!("  verification      - Review and verification");
            println!("  claude_code_guide - Questions about Claude Code");
            println!("  statusline_setup  - Status line configuration");
        } else {
            println!(
                "  general_purpose  explore  plan  verification  claude_code_guide  statusline_setup"
            );
        }

        // Custom agents are user files; listing them is the discoverability
        // half of resolving `--agent <name>` and the Agent tool's `agent`.
        let custom = coderex_agents::custom_agents::load_custom_agents();
        let mut names: Vec<&str> = custom.keys().map(String::as_str).collect();
        names.sort_unstable();
        println!();
        println!("Custom agents (~/.coderex/agents/*.json):");
        if names.is_empty() {
            println!("  none configured");
        } else if detailed {
            for name in names {
                if let Some(def) = custom.get(name) {
                    let tools = if def.allowed_tools.is_empty() {
                        "all tools".to_string()
                    } else {
                        def.allowed_tools.join(", ")
                    };
                    let summary = def.system_prompt.lines().next().unwrap_or("");
                    println!("  {name:<17} - {summary} (tools: {tools})");
                }
            }
        } else {
            println!("  {}", names.join("  "));
        }
        Ok(())
    }

    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn run_auth(&self) -> Result<()> {
        let mut config = coderex_core::provider_config::ProviderConfig::from_env();
        // Apply CLI --api-key override if present
        if let Some(key) = self.api_key_from_settings() {
            config.set_anthropic_api_key(key.into());
        }
        let mut providers = Vec::new();
        if config.anthropic.is_some() {
            providers.push("Anthropic");
        }
        if config.openai.is_some() {
            providers.push("OpenAI");
        }
        if config.groq.is_some() {
            providers.push("Groq");
        }
        if !config.custom.is_empty() {
            providers.push("Custom");
        }
        if providers.is_empty() {
            println!("No API providers configured.");
            println!("Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or GROQ_API_KEY.");
        } else {
            println!("Authenticated providers: {}", providers.join(", "));
        }
        Ok(())
    }

    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn run_doctor(&self) -> Result<()> {
        println!("CodeRex Doctor");
        println!("==============");
        let checks = [
            (
                "API key configured",
                Self::try_create_api_client(
                    self.api_key_from_settings(),
                    self.prompt_caching_from_settings(),
                    self.provider_settings(),
                )
                .is_some(),
            ),
            (
                "Settings file exists",
                coderex_config::config_home().join("settings.json").exists(),
            ),
            (
                "Working directory accessible",
                std::env::current_dir().is_ok(),
            ),
        ];
        let mut issues = 0;
        for (name, ok) in &checks {
            let status = if *ok { "OK" } else { "FAIL" };
            println!("  [{status}] {name}");
            if !ok {
                issues += 1;
            }
        }
        if issues == 0 {
            println!("\nAll checks passed.");
        } else {
            println!("\n{issues} check(s) failed.");
        }
        Ok(())
    }

    async fn run_update(&self, check: bool) -> Result<()> {
        use crate::{self_update, version};
        let cwd = self.ctx.cwd.clone();
        let report =
            match self_update::update(self_update::UpdateOptions { check_only: check }).await {
                Ok(report) => report,
                Err(error) => {
                    eprintln!("Update failed: {error}");
                    std::process::exit(1);
                }
            };
        match &report.installed {
            Some(path) => println!(
                "Updated {} -> {} at {}",
                report.current,
                report.latest.as_deref().unwrap_or("the latest release"),
                path.display()
            ),
            None => match (report.update_available, &report.latest) {
                (true, Some(latest)) => println!(
                    "New version available: {latest} (current: {}). Run `coderex update` to install it.",
                    report.current
                ),
                (true, None) => println!(
                    "A newer version may be available; the release lookup did not return one."
                ),
                (false, Some(latest)) => println!("CodeRex is up to date ({latest})."),
                (false, None) => println!("CodeRex is up to date."),
            },
        }
        version::record_check_time(std::path::Path::new(&cwd));
        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn run_mcp(&self, action: Option<crate::cli::McpAction>) -> Result<()> {
        let mcp_path = self
            .ctx
            .mcp_config_path
            .clone()
            .unwrap_or_else(|| std::path::Path::new(&self.ctx.cwd).join(".mcp.json"));
        match action {
            Some(crate::cli::McpAction::List) => {
                println!("Configured MCP servers:");
                if mcp_path.exists() {
                    match coderex_mcp::config::McpConfig::load(&mcp_path) {
                        Ok(config) => {
                            if config.mcp_servers.is_empty() {
                                println!("  (none)");
                            } else {
                                for (name, srv) in &config.mcp_servers {
                                    if let Some(url) = &srv.url {
                                        println!("  {name}: {:?} {url}", srv.transport);
                                    } else {
                                        println!(
                                            "  {name}: {} {}",
                                            srv.command,
                                            srv.args.join(" ")
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => println!("  Error loading config: {e}"),
                    }
                } else {
                    println!("  No MCP config found at {}", mcp_path.display());
                }
            }
            Some(crate::cli::McpAction::Add {
                name,
                command,
                url,
                transport,
                header,
            }) => {
                let mut config = if mcp_path.exists() {
                    coderex_mcp::config::McpConfig::load(&mcp_path)
                        .unwrap_or_else(|_| coderex_mcp::config::McpConfig::new())
                } else {
                    coderex_mcp::config::McpConfig::new()
                };
                let added = if let Some(url) = url {
                    let transport = match transport.as_deref().unwrap_or("http") {
                        "http" | "streamable-http" => coderex_mcp::config::McpTransport::Http,
                        "sse" => coderex_mcp::config::McpTransport::Sse,
                        other => {
                            println!("Error: unsupported transport '{other}' (use http or sse)");
                            return Ok(());
                        }
                    };
                    let mut headers = std::collections::HashMap::new();
                    for entry in &header {
                        let Some((key, value)) = entry.split_once('=') else {
                            println!("Error: --header expects KEY=VALUE, got '{entry}'");
                            return Ok(());
                        };
                        headers.insert(key.trim().to_string(), value.trim().to_string());
                    }
                    config.add_remote_server(name.clone(), url, transport, headers)
                } else if let Some(command) = command {
                    config.add_stdio_server(name.clone(), &command)
                } else {
                    Err("give a command or --url".to_string())
                };
                match added {
                    Ok(()) => match config.save(&mcp_path) {
                        Ok(()) => println!("Added MCP server '{name}' to {}", mcp_path.display()),
                        Err(e) => println!("Error saving config: {e}"),
                    },
                    Err(e) => println!("Error: {e}"),
                }
            }
            Some(crate::cli::McpAction::Remove { name }) => {
                let mut config = if mcp_path.exists() {
                    coderex_mcp::config::McpConfig::load(&mcp_path)
                        .unwrap_or_else(|_| coderex_mcp::config::McpConfig::new())
                } else {
                    coderex_mcp::config::McpConfig::new()
                };
                if config.remove_server(&name).is_some() {
                    match config.save(&mcp_path) {
                        Ok(()) => {
                            println!("Removed MCP server '{name}' from {}", mcp_path.display());
                        }
                        Err(e) => println!("Error saving config: {e}"),
                    }
                } else {
                    println!("MCP server '{name}' not found in config.");
                }
            }
            Some(crate::cli::McpAction::Test { name }) => {
                if !mcp_path.exists() {
                    println!("No MCP config found at {}", mcp_path.display());
                    return Ok(());
                }
                let config = match coderex_mcp::config::McpConfig::load(&mcp_path) {
                    Ok(config) => config,
                    Err(e) => {
                        println!("Error loading config: {e}");
                        return Ok(());
                    }
                };
                let Some(server_config) = config.mcp_servers.get(&name).cloned() else {
                    println!("MCP server '{name}' not found in config.");
                    return Ok(());
                };

                // Start exactly the named server. `load_config` inserts it
                // into a fresh manager, so unrelated or broken servers in the
                // same config file cannot affect this test.
                let mut selected = coderex_mcp::config::McpConfig::new();
                selected.mcp_servers.insert(name.clone(), server_config);
                let manager = coderex_mcp::manager::McpManager::new();
                manager.load_config(&selected);
                manager.start_all().await;

                let running = manager.is_running(&name);
                let tools: Vec<coderex_mcp::proxy::McpToolInfo> = manager
                    .list_tool_info()
                    .into_iter()
                    .filter(|tool| tool.server == name)
                    .collect();
                let status = manager.get_server(&name).map_or_else(
                    || "not registered".to_string(),
                    |server| format!("{:?}", server.status),
                );

                // Always tear the connection down, including on failure.
                manager.stop_all().await;

                if !running {
                    bail!(
                        "MCP server '{name}' did not start (status: {status}). \
                         Check the command/URL and the server's stderr."
                    );
                }

                // "Started", not "connected": a stdio process can be alive
                // while its initialize handshake failed. The tool count below
                // is the evidence of whether it is actually usable.
                println!("MCP server '{name}' started (status: {status}).");
                println!("Tools discovered: {}", tools.len());
                for tool in &tools {
                    if tool.description.trim().is_empty() {
                        println!(
                            "  {} (server: {}, tool: {})",
                            tool.qualified_name(),
                            tool.server,
                            tool.name
                        );
                    } else {
                        println!("  {} — {}", tool.qualified_name(), tool.description.trim());
                    }
                }
            }
            None => {
                println!("Usage: coderex mcp <list|add|remove|test>");
            }
        }
        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn run_plugin(&self, action: Option<crate::cli::PluginAction>) -> Result<()> {
        let install_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/plugins");
        match action {
            Some(crate::cli::PluginAction::List) => {
                if let Some(ref pm) = self.ctx.plugin_manager {
                    let plugins = pm.list_plugins();
                    if plugins.is_empty() {
                        println!("No plugins installed.");
                    } else {
                        println!("Installed plugins:");
                        for name in plugins {
                            println!("  {name}");
                        }
                    }
                } else {
                    println!("No plugins installed.");
                }
            }
            Some(crate::cli::PluginAction::Install { source }) => {
                println!("Installing plugin from '{source}'...");
                let manager = plugin_manager_with_signing_key();
                // Read plugin host configuration from the settings files
                // directly. `ctx.plugin_settings`/`ctx.marketplaces` are
                // populated by the startup phases, but this subcommand must
                // also work when it is reached without them.
                let plugin_host = self.plugin_host_config_service().load_plugin_host_config();
                let plugin_settings = self.ctx.plugin_settings.clone().or(plugin_host.plugins);
                // Resolve host `userConfig` values before installing; the
                // install path consults them when materialising the plugin.
                if let Some(plugins) = plugin_settings.as_ref() {
                    match manager.apply_options_from_settings(plugins) {
                        Ok(count) => tracing::info!(
                            "applied userConfig for {count} plugin option(s) before install"
                        ),
                        Err(error) => tracing::warn!("plugin userConfig ignored: {error}"),
                    }
                }
                // Settings-configured marketplaces first, then the startup
                // view, then the legacy environment pair; duplicates collapse
                // in `configured_marketplaces`. Registration happens on the
                // same manager that performs the install, so a bare name
                // resolves against these marketplaces.
                let mut configured = plugin_host.marketplaces;
                configured.extend(self.ctx.marketplaces.iter().cloned());
                let marketplaces = Self::configured_marketplaces(&configured);
                Self::register_marketplaces(&manager, &marketplaces);

                // A bare name is a marketplace listing; a URL, git remote, or
                // existing directory is a source. `PluginSource` is the same
                // classifier `install_plugin` uses internally.
                let detected = coderex_plugins::plugins::PluginSource::detect(&source);
                let result = if detected.is_marketplace() {
                    manager.install_plugin_by_name(&source, &install_dir).await
                } else {
                    let plugin = coderex_plugins::plugins::MarketplacePlugin {
                        name: source.clone(),
                        version: "latest".into(),
                        url: source.clone(),
                        description: String::new(),
                    };
                    manager.install_plugin(&plugin, &install_dir).await
                };
                match result {
                    Ok(path) => println!("Plugin installed to: {}", path.display()),
                    // A failed install must not exit 0: scripts and CI read
                    // the status, and a refusal that reports success is a
                    // refusal nobody acts on.
                    Err(e) => {
                        println!("Installation failed: {e}");
                        return Err(anyhow::anyhow!("plugin install failed: {e}"));
                    }
                }
            }
            Some(crate::cli::PluginAction::Uninstall { name }) => {
                println!("Uninstalling plugin '{name}'...");
                if let Some(ref pm) = self.ctx.plugin_manager {
                    if pm.unload_plugin(&name) {
                        // Remove plugin directory
                        let plugin_dir = install_dir.join(&name);
                        if plugin_dir.exists() {
                            match std::fs::remove_dir_all(&plugin_dir) {
                                Ok(()) => println!("Plugin '{name}' uninstalled."),
                                Err(e) => println!("Failed to remove plugin directory: {e}"),
                            }
                        } else {
                            println!("Plugin '{name}' removed from registry.");
                        }
                    } else {
                        println!("Plugin '{name}' not found.");
                    }
                } else {
                    println!("No plugin manager available.");
                }
            }
            Some(crate::cli::PluginAction::Update) => {
                println!("Updating plugins...");
                let manager = plugin_manager_with_signing_key();
                let reconciler =
                    coderex_plugins::plugins::MarketplaceReconciler::with_auto_install_enabled(
                        300,
                        Self::plugin_auto_install_enabled(),
                    );
                match reconciler.reconcile(&manager, Some(&install_dir)).await {
                    Ok(report) => {
                        println!(
                            "Update complete: {} new, {} updated, {} removed",
                            report.new_count, report.updated_count, report.removed_count
                        );
                    }
                    Err(e) => println!("Update failed: {e}"),
                }
            }
            None => {
                println!("Usage: coderex plugin <list|install|uninstall|update>");
            }
        }
        Ok(())
    }

    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn run_auto_mode(&self) -> Result<()> {
        println!("Auto-mode settings:");
        let memory_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/memory");
        let dream_config_path = memory_dir.join("dream_config.json");
        if dream_config_path.exists() {
            match std::fs::read_to_string(&dream_config_path) {
                Ok(content) => {
                    println!("  Config file: {}", dream_config_path.display());
                    println!("  {content}");
                }
                Err(e) => println!("  Config exists but unreadable: {e}"),
            }
        } else {
            println!("  Auto-dream: disabled (no config found)");
            println!("  Use /auto-dream enable inside the REPL to enable.");
        }
        Ok(())
    }

    /// Inspect telemetry state without uploading anything.
    // Retained `async`: the startup/dispatch pipeline awaits these on the current
    // task, and removing it cascades into unnecessary_wraps/unused_self.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn run_telemetry(&self, action: Option<crate::cli::TelemetryAction>) -> Result<()> {
        match action {
            Some(crate::cli::TelemetryAction::PreviewCrash { file }) => {
                let (verbose, strict) = self.telemetry_redaction_modes();
                Self::preview_crash(file, verbose, strict)
            }
            None => {
                println!("Usage: coderex telemetry preview-crash [FILE]");
                Ok(())
            }
        }
    }

    /// Print the exact wire payload the uploader would send for one crash.
    ///
    /// The pending file is read but not removed or ingested, and nothing is
    /// uploaded. `verbose` and `strict` come from
    /// [`telemetry_redaction_modes`](Self::telemetry_redaction_modes), which
    /// resolves them the way the uploader does — settings included.
    fn preview_crash(file: Option<std::path::PathBuf>, verbose: bool, strict: bool) -> Result<()> {
        let pending_dir = coderex_config::config_home()
            .join("crashes")
            .join("pending");
        let path = match file {
            Some(path) => path,
            None => Self::latest_pending_crash(&pending_dir)?.ok_or_else(|| {
                anyhow::anyhow!("no pending crash reports in {}", pending_dir.display())
            })?,
        };

        let bytes = std::fs::read(&path)
            .with_context(|| format!("cannot read pending crash {}", path.display()))?;
        let report: coderex_telemetry::CrashReport = serde_json::from_slice(&bytes)
            .with_context(|| format!("cannot parse pending crash {}", path.display()))?;

        // The reporter supplies the ingestion modes (verbose/strict), so the
        // row shape matches what a real ingestion would have stored.
        let reporter = coderex_telemetry::CrashReporter::new(pending_dir);
        let pending = reporter.pending_crash_from_report(&report);
        let payload = coderex_telemetry::preview_crash_payload_with(&pending, verbose, strict);
        println!("{}", serde_json::to_string_pretty(&payload)?);
        Ok(())
    }

    /// The newest `*.json` pending crash, by modification time then name.
    fn latest_pending_crash(dir: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "cannot read crash pending directory {}: {error}",
                    dir.display()
                ));
            }
        };

        let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!("cannot read crash pending directory {}", dir.display())
            })?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let replace = match &newest {
                Some((current, current_path)) => {
                    modified > *current || (modified == *current && path > *current_path)
                }
                None => true,
            };
            if replace {
                newest = Some((modified, path));
            }
        }
        Ok(newest.map(|(_, path)| path))
    }

    // The future borrows the non-`Sync` REPL loop, so it is not `Send`; it is
    // awaited from `run` on the current task and never spawned.
    #[allow(clippy::future_not_send)]
    #[allow(clippy::unused_async)]
    async fn run_resume(&self, cwd: &str, session_id: &str, model: ModelId) -> Result<()> {
        let resolved_session_id = if session_id.eq_ignore_ascii_case("latest") {
            session_resume::latest_session_id(cwd).ok_or_else(|| {
                anyhow::anyhow!("No transcript found to resume in .coderex/transcripts")
            })?
        } else {
            session_id.to_string()
        };
        let messages = Self::load_resume_messages(cwd, &resolved_session_id)?;

        tracing::info!(
            "Resuming session {}: {} messages loaded",
            resolved_session_id,
            messages.len()
        );
        println!("Resuming session: {resolved_session_id}");
        println!("Loaded {} messages from transcript", messages.len());

        // Show a summary of the conversation history
        let user_msgs: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m.role, coderex_core::types::Role::User))
            .collect();
        println!("User messages in session: {}", user_msgs.len());
        for (i, msg) in user_msgs.iter().enumerate() {
            if let coderex_core::types::MessageContent::Text(s) = &msg.content {
                let preview = if s.len() > 80 {
                    format!("{}...", &s[..77])
                } else {
                    s.clone()
                };
                println!("  {i}. {preview}");
            }
        }

        if !std::io::stdin().is_terminal() {
            bail!("resume requires an interactive terminal on stdin");
        }

        // Enter REPL with the resumed context
        let api_key = self.api_key_from_settings();
        let prompt_caching = self.prompt_caching_from_settings();
        let mut repl = if let Some(api_client) =
            Self::try_create_api_client(api_key, prompt_caching, self.provider_settings())
        {
            let api_client = Self::apply_effort(api_client, self.ctx.effort.as_deref());
            self.install_sampling_handler(api_client.as_ref(), &model);
            crate::repl::ReplLoop::with_engine(api_client, cwd.to_string(), model)
        } else {
            crate::repl::ReplLoop::new(cwd.to_string(), model)
        };
        repl.with_resume_session(Some(resolved_session_id));
        if let Some(ref pm) = self.ctx.plugin_manager {
            repl.with_plugin_manager(Arc::clone(pm));
        }
        self.configure_repl(&mut repl, true);
        repl.run_tui().await
    }

    fn load_resume_messages(
        cwd: &str,
        session_id: &str,
    ) -> Result<Vec<coderex_core::types::Message>> {
        session_resume::load_resume_messages(cwd, session_id)
    }

    /// Find the most recent session ID from the transcripts directory.
    fn find_latest_session(&self) -> Option<String> {
        session_resume::latest_session_id(&self.ctx.cwd)
    }
}

/// Run the teleport rendezvous service until the process is stopped.
///
/// A session shared with `/teleport --share` is uploaded here, and any machine
/// pointed at this service with `CODEREX_TELEPORT_URL` can join it. The
/// service keeps no history: a share lives until it is revoked or its time to
/// live runs out.
async fn run_teleport_service(port: u16, ttl_secs: u64) -> Result<()> {
    let ttl = std::time::Duration::from_secs(ttl_secs);
    let service = coderex_server::teleport::TeleportService::with_ttl(port, ttl);
    println!(
        "Serving teleport shares on http://0.0.0.0:{port}/teleport/shares (ttl {ttl_secs}s). \
         Point clients at it with CODEREX_TELEPORT_URL."
    );
    service.run().await.map_err(|error| anyhow::anyhow!(error))
}

/// Type-check the exported plugin declarations and report what the compiler said.
///
/// This is the on-demand half of the startup check: the same compiler, the
/// same outcome, but printed for a person and with an exit status.
fn run_plugin_typecheck(dir: Option<std::path::PathBuf>) {
    let root = dir.unwrap_or_else(|| coderex_config::config_home().join("plugin-types"));
    let outcome =
        coderex_plugins::typecheck::typecheck(&root, coderex_plugins::typecheck::DEFAULT_TIMEOUT);
    match &outcome {
        coderex_plugins::typecheck::TypeCheckOutcome::Passed => {
            println!("{}: {}", root.display(), outcome.summary());
        }
        coderex_plugins::typecheck::TypeCheckOutcome::Failed(diagnostics) => {
            println!("{} type error(s) in {}", diagnostics.len(), root.display());
            for diagnostic in diagnostics {
                println!("  {}", diagnostic.render());
            }
            std::process::exit(1);
        }
        // "Unavailable" is not a failure: there was nothing to check with, and
        // saying "failed" would blame the plugin for the host's toolchain.
        coderex_plugins::typecheck::TypeCheckOutcome::Unavailable(reason) => {
            println!("nothing checked: {reason}");
        }
        coderex_plugins::typecheck::TypeCheckOutcome::TimedOut(limit) => {
            eprintln!("the compiler did not finish within {}s", limit.as_secs());
            std::process::exit(2);
        }
    }
}

/// Run the bridge gateway until the process is stopped.
///
/// One process serves the control plane a worker registers with and pulls
/// assignments from, the endpoint an operator submits work to, and the
/// per-environment frame mailbox that carries session output and permission
/// requests to that operator (and the answers back).
async fn run_gateway(port: u16) -> Result<()> {
    println!(
        "Serving the bridge gateway on http://0.0.0.0:{port} \
         (/api/environments/register, /api/environments/<env>/assignments, \
         /api/environments/<env>/frames)."
    );
    coderex_server::gateway::GatewayServer::new(port)
        .run()
        .await
        .map_err(|error| anyhow::anyhow!(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_permission_control_gate_needs_print_stream_json_and_a_positional_prompt() {
        // The only combination with both an output channel and a free stdin.
        assert!(headless_permission_control_allowed(
            true,
            "stream-json",
            false
        ));
        // A prompt read from stdin leaves no stdin for the answer.
        assert!(!headless_permission_control_allowed(
            true,
            "stream-json",
            true
        ));
        // Interactive mode owns stdin and has its own confirmation UI.
        assert!(!headless_permission_control_allowed(
            false,
            "stream-json",
            false
        ));
        // No stream-json sink means nowhere to emit the request.
        assert!(!headless_permission_control_allowed(true, "text", false));
    }

    #[test]
    fn settings_mcp_servers_accept_both_shapes() {
        let bare = serde_json::json!({
            "files": { "command": "npx", "args": ["-y", "server"] }
        });
        let parsed = StartupOrchestrator::parse_settings_mcp_servers(&bare).unwrap();
        assert_eq!(parsed.server_names(), vec!["files"]);

        let wrapped = serde_json::json!({
            "mcpServers": {
                "remote": { "type": "http", "url": "https://example.test/mcp" }
            }
        });
        let parsed = StartupOrchestrator::parse_settings_mcp_servers(&wrapped).unwrap();
        assert_eq!(parsed.server_names(), vec!["remote"]);

        let empty =
            StartupOrchestrator::parse_settings_mcp_servers(&serde_json::json!({})).unwrap();
        assert!(empty.is_empty());
        let null =
            StartupOrchestrator::parse_settings_mcp_servers(&serde_json::Value::Null).unwrap();
        assert!(null.is_empty());
    }

    #[test]
    fn a_plugin_budget_can_only_tighten_the_ceiling() {
        let mut orch = StartupOrchestrator::new();
        let settings = Settings {
            max_budget_usd: Some(10.0),
            ..Default::default()
        };
        orch.ctx.settings = Some(settings);

        orch.apply_plugin_settings(&serde_json::json!({"max_budget_usd": 5.0}));
        assert_eq!(
            orch.ctx.settings.as_ref().unwrap().max_budget_usd,
            Some(5.0)
        );

        // A plugin cannot raise what the user set.
        orch.apply_plugin_settings(&serde_json::json!({"max_budget_usd": 99.0}));
        assert_eq!(
            orch.ctx.settings.as_ref().unwrap().max_budget_usd,
            Some(5.0)
        );
    }

    #[test]
    fn plugin_settings_deep_merge_and_keep_the_user_value() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings {
            model: Some("user-model".into()),
            vim_mode: Some(true),
            ..Default::default()
        });

        // The user set `model` and `vim_mode`; the plugin fills the rest.
        orch.apply_plugin_settings(&serde_json::json!({
            "model": "plugin-model",
            "vim_mode": false,
            "statusline": {"format": "hi"},
        }));

        let settings = orch.ctx.settings.as_ref().unwrap();
        assert_eq!(
            settings.model.as_deref(),
            Some("user-model"),
            "a user value outranks the plugin"
        );
        assert_eq!(settings.vim_mode, Some(true));
        assert_eq!(
            settings.statusline.as_ref().unwrap().format.as_deref(),
            Some("hi"),
            "a key the user never set takes the plugin's value"
        );
    }

    #[test]
    fn plugin_settings_fill_a_key_the_user_left_unset() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings::default());
        orch.apply_plugin_settings(&serde_json::json!({"model": "plugin-model"}));
        assert_eq!(
            orch.ctx.settings.as_ref().unwrap().model.as_deref(),
            Some("plugin-model")
        );
    }

    #[test]
    fn plugin_settings_refuse_security_sensitive_keys() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings {
            permission_mode: Some("default".into()),
            telemetry_enabled: Some(true),
            ..Default::default()
        });

        orch.apply_plugin_settings(&serde_json::json!({
            "hooks": {"PreToolUse": []},
            "mcpServers": {"evil": {"command": "sh"}},
            "api_key": "sk-plugin",
            "env": {"LD_PRELOAD": "/tmp/x.so"},
            "permission_mode": "bypass-permissions",
            "telemetry_enabled": false,
            "permissions": {"allow": ["Bash"]},
        }));

        let settings = orch.ctx.settings.as_ref().unwrap();
        // The permission mode may only tighten, never loosen.
        assert_eq!(settings.permission_mode.as_deref(), Some("default"));
        // Hooks, MCP servers, keys and env are never taken.
        assert!(settings.hooks.is_none());
        assert!(settings.mcp_servers.is_none());
        assert!(settings.api_key.is_none());
        assert!(
            settings.permissions.is_none(),
            "a plugin must not set rules"
        );
        // Telemetry may be turned off by a plugin, never on; here it was on
        // and the plugin asked to turn it off, which is allowed.
        assert_eq!(settings.telemetry_enabled, Some(false));
    }

    #[test]
    fn plugin_settings_may_tighten_the_permission_mode() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings {
            permission_mode: Some("accept-edits".into()),
            ..Default::default()
        });
        orch.apply_plugin_settings(&serde_json::json!({"permission_mode": "plan"}));
        assert_eq!(
            orch.ctx
                .settings
                .as_ref()
                .unwrap()
                .permission_mode
                .as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn plugin_settings_ignore_unknown_keys() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings::default());
        orch.apply_plugin_settings(&serde_json::json!({"not_a_setting": true}));
        // The unknown key changes nothing and does not corrupt the rest.
        assert!(orch.ctx.settings.as_ref().unwrap().model.is_none());
    }

    /// Every key the resolved `Settings` understands is governed by one of the
    /// three plugin-settings rules, so none of them is ever warned about as
    /// "unknown". This is the invariant the merge doc comment claims.
    #[test]
    fn every_settings_key_is_governed_by_a_plugin_settings_rule() {
        let mergeable = mergeable_settings_keys();
        let governed: std::collections::HashSet<&str> = PLUGIN_SETTINGS_NEVER_TAKEN
            .iter()
            .copied()
            .chain(PLUGIN_SETTINGS_TIGHTEN_ONLY.iter().copied())
            .chain(mergeable.iter().map(String::as_str))
            .collect();

        let keys: Vec<String> = serde_json::to_value(settings_with_every_key())
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert!(keys.contains(&"context_window".to_string()), "{keys:?}");
        for key in &keys {
            assert!(
                governed.contains(key.as_str()),
                "settings key `{key}` is neither refused, tightened nor merged"
            );
        }
    }

    /// Widening the merge covers every non-policy key `Settings` understands —
    /// not just a hand-written four — while the never-taken and tighten-only
    /// keys stay out of it.
    #[test]
    fn plugin_settings_merge_covers_every_non_policy_key() {
        let mergeable = mergeable_settings_keys();
        let keys: Vec<String> = serde_json::to_value(settings_with_every_key())
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for key in &keys {
            let policy = PLUGIN_SETTINGS_NEVER_TAKEN.contains(&key.as_str())
                || PLUGIN_SETTINGS_TIGHTEN_ONLY.contains(&key.as_str());
            assert_eq!(
                mergeable.contains(key),
                !policy,
                "`{key}` should {} merge",
                if policy { "not" } else { "" }
            );
        }
        // A plugin can still only tighten the policy keys; they are not merged.
        for key in PLUGIN_SETTINGS_TIGHTEN_ONLY {
            assert!(!mergeable.contains(*key), "{key} must not merge");
        }
    }

    /// A key a plugin supplies that `Settings` understands and the policy does
    /// not govern is merged into the session.
    #[test]
    fn plugin_settings_merge_a_non_policy_key_the_user_left_unset() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings::default());
        orch.apply_plugin_settings(&serde_json::json!({
            "context_window": {"default": 1234},
        }));
        let settings = orch.ctx.settings.as_ref().unwrap();
        assert_eq!(
            settings
                .context_window
                .as_ref()
                .and_then(|window| window.default),
            Some(1234),
            "a non-policy key the user never set takes the plugin's value"
        );
    }

    #[test]
    fn plugin_channels_are_validated_and_listed() {
        let manager = coderex_plugins::plugins::PluginManager::new();
        let manifest: coderex_plugins::plugins::PluginManifest =
            serde_json::from_value(serde_json::json!({
                "name": "chan",
                "version": "1.0.0",
                "mcpServers": {
                    "chat": { "command": "chat-server" }
                },
                "channels": [
                    { "server": "chat", "displayName": "Team chat" },
                    { "server": "missing", "displayName": "Nowhere" }
                ]
            }))
            .expect("the manifest should parse");
        manager.load_plugin(manifest, std::path::PathBuf::from("/tmp/chan"));

        let mut orch = StartupOrchestrator::new();
        orch.ctx.plugin_manager = Some(Arc::new(manager));
        let mut repl = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_plugin_components(&mut repl, orch.ctx.plugin_manager.as_ref().unwrap());

        let components = repl.command_context_for_test().plugin_components;
        assert!(
            components
                .iter()
                .any(|(kind, name)| kind == "channel" && name.contains("Team chat -> chat")),
            "{components:?}"
        );
        assert!(
            components
                .iter()
                .any(|(kind, name)| kind == "channel" && name.contains("Nowhere -> missing")),
            "a channel naming an unknown server is still listed: {components:?}"
        );
    }

    /// The production subscriber moves a configured channel off `Idle` and
    /// records what a routed notification delivers.
    #[test]
    fn subscribing_channels_consumes_a_configured_channels_notifications() {
        let manager = coderex_plugins::plugins::PluginManager::new();
        let manifest: coderex_plugins::plugins::PluginManifest =
            serde_json::from_value(serde_json::json!({
                "name": "chan",
                "version": "1.0.0",
                "mcpServers": {"chat": {"command": "chat-server"}},
                "channels": [{"server": "chat", "displayName": "Team chat"}]
            }))
            .expect("the manifest should parse");
        manager.load_plugin(manifest, std::path::PathBuf::from("/tmp/chan"));

        let manager = Arc::new(manager);
        // With no subscriber the channel is honestly idle.
        assert_eq!(
            manager.plugin_channel_report()[0].status,
            coderex_plugins::plugins::ChannelStatus::Idle
        );

        let log = StartupOrchestrator::subscribe_plugin_channels(&manager);
        let report = manager.plugin_channel_report();
        assert_eq!(
            report[0].status,
            coderex_plugins::plugins::ChannelStatus::Subscribed,
            "a configured channel with a subscriber is no longer idle"
        );
        assert_eq!(report[0].subscribers, 1);

        let delivered = manager.route_channel_notification(
            "chat",
            "notifications/message",
            Some(&serde_json::json!({"level": "info"})),
        );
        assert_eq!(delivered, 1);

        let events = log.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].channel, "chat");
        assert_eq!(events[0].plugin, "chan");
        assert_eq!(events[0].server, "chat");
        assert_eq!(events[0].method, "notifications/message");
        assert_eq!(events[0].params.as_ref().unwrap()["level"], "info");
    }

    /// The CLI's router hands an MCP notification to the channel layer, and a
    /// server no channel names delivers to nobody.
    #[test]
    fn the_channel_router_delivers_to_subscribed_channels_only() {
        let manager = coderex_plugins::plugins::PluginManager::new();
        let manifest: coderex_plugins::plugins::PluginManifest =
            serde_json::from_value(serde_json::json!({
                "name": "chan",
                "version": "1.0.0",
                "mcpServers": {"chat": {"command": "chat-server"}},
                "channels": [{"server": "chat"}]
            }))
            .expect("the manifest should parse");
        manager.load_plugin(manifest, std::path::PathBuf::from("/tmp/chan"));

        let manager = Arc::new(manager);
        let log = StartupOrchestrator::subscribe_plugin_channels(&manager);
        let router = PluginChannelRouter {
            manager: Arc::clone(&manager),
        };

        coderex_mcp::manager::ServerNotificationRouter::route(
            &router,
            "chat",
            "notifications/message",
            Some(&serde_json::json!({"level": "info"})),
        );
        assert_eq!(log.lock().unwrap().len(), 1);

        // A notification from a server no channel binds to goes nowhere.
        coderex_mcp::manager::ServerNotificationRouter::route(
            &router,
            "unbound",
            "notifications/message",
            None,
        );
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    /// The registry's real consumer: `/plugin components` lists a declared
    /// `.d.ts`, so the session's component view is what reads it back.
    #[test]
    fn plugin_type_declarations_are_listed_in_the_components() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_root = dir.path().join("typed");
        std::fs::create_dir_all(plugin_root.join("types")).unwrap();
        std::fs::write(
            plugin_root.join("types/index.d.ts"),
            "declare module 'coderex' { export const x: number }",
        )
        .unwrap();

        let manager = coderex_plugins::plugins::PluginManager::new();
        let manifest: coderex_plugins::plugins::PluginManifest =
            serde_json::from_value(serde_json::json!({
                "name": "typed",
                "version": "1.0.0",
                "types": {"main": "types/index.d.ts"}
            }))
            .expect("the manifest should parse");
        manager.load_plugin(manifest, plugin_root);

        let mut orch = StartupOrchestrator::new();
        orch.ctx.plugin_manager = Some(Arc::new(manager));
        let mut repl = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_plugin_components(&mut repl, orch.ctx.plugin_manager.as_ref().unwrap());

        let components = repl.command_context_for_test().plugin_components;
        assert!(
            components
                .iter()
                .any(|(kind, name)| kind == "types" && name.contains("main (typed)")),
            "the declared declaration is listed: {components:?}"
        );
    }

    #[test]
    fn configure_repl_installs_plugin_hooks_as_their_own_tier() {
        let manager = coderex_plugins::plugins::PluginManager::new();
        let manifest: coderex_plugins::plugins::PluginManifest =
            serde_json::from_value(serde_json::json!({
                "name": "hooky",
                "version": "1.0.0",
                "hooks": [{
                    "event": "PreToolUse",
                    "hooks": [{ "type": "command", "command": "true" }]
                }]
            }))
            .expect("the manifest should parse");
        manager.load_plugin(manifest, std::path::PathBuf::from("/tmp/hooky"));

        let mut orch = StartupOrchestrator::new();
        orch.ctx.plugin_manager = Some(Arc::new(manager));
        let mut repl = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_repl(&mut repl, true);

        let layers = repl
            .hook_layers_for_test()
            .expect("plugin hooks should create a layer document");
        let plugin = layers.plugin.as_ref().expect("the Plugin tier is empty");
        let entries = plugin
            .get("hooks")
            .and_then(serde_json::Value::as_array)
            .expect("the tier carries a hooks array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["event"], "PreToolUse");
    }

    #[test]
    fn configure_repl_enables_the_control_channel_only_for_headless() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.headless_permission_control = true;

        let mut interactive = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_repl(&mut interactive, true);
        assert!(
            !interactive.headless_permission_control(),
            "interactive mode must keep the confirmation UI"
        );

        let mut headless = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_repl(&mut headless, false);
        assert!(headless.headless_permission_control());
    }

    #[test]
    fn session_context_defaults() {
        let ctx = SessionContext::default();
        assert!(ctx.settings.is_none());
        assert!(ctx.model.is_none());
        assert_eq!(ctx.cwd, "");
    }

    #[test]
    fn startup_orchestrator_defaults() {
        let orch = StartupOrchestrator::new();
        assert!(orch.ctx.settings.is_none());
        #[cfg(feature = "enterprise")]
        assert!(orch.ctx.remote_barrier.is_none());
    }

    #[tokio::test]
    async fn bootstrap_phase_succeeds() {
        let orch = StartupOrchestrator::new();
        orch.phase_bootstrap().await.unwrap();
    }

    fn test_cli_args() -> Cli {
        Cli {
            directory: None,
            add_dir: Vec::new(),
            permissions: None,
            dangerously_skip_permissions: false,
            allowed_tools: None,
            model: None,
            fallback_model: None,
            system_prompt: None,
            append_system_prompt: None,
            file: Vec::new(),
            print: false,
            brief: false,
            bare: false,
            verbose: false,
            debug: false,
            debug_file: None,
            r#continue: false,
            resume: None,
            name: None,
            fork_session: None,
            from_pr: None,
            no_session_persistence: false,
            disable_slash_commands: false,
            max_budget_usd: None,
            max_turns: None,
            api_key: None,
            no_telemetry: false,
            mcp_config: None,
            mcp_debug: false,
            strict_mcp_config: false,
            plugin_dir: None,
            settings: None,
            setting_sources: false,
            input_format: "text".into(),
            output_format: "text".into(),
            json_schema: None,
            replay_user_messages: false,
            include_partial_messages: false,
            effort: None,
            betas: Vec::new(),
            chrome: false,
            no_chrome: false,
            tmux: false,
            agent: None,
            worktree: false,
            prompt: Vec::new(),
            command: None,
        }
    }

    /// The CLI's retry loop is what turns a recovered manifest into a running
    /// plugin: `retry_failed_plugins` only says `RestartRequired`, because it
    /// is synchronous and cannot await `spawn_plugin`. This drives the loop
    /// against a real process plugin that starts broken and is then fixed.
    ///
    /// Unix-only: the plugin is a `sh` script made executable with `set_mode`.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_restart_loop_spawns_a_plugin_that_recovered() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("dir");
        let plugin_dir = dir.path().join("recovering");
        std::fs::create_dir_all(plugin_dir.join("bin")).expect("plugin dir");
        // Start with a plugin that cannot spawn, so it lands in `Error` the
        // way a crashed runtime does.
        std::fs::write(
            plugin_dir.join("plugin.json"),
            serde_json::json!({
                "name": "recovering",
                "version": "1.0.0",
                "entrypoint": "bin/missing",
            })
            .to_string(),
        )
        .expect("write manifest");

        let manager = Arc::new(coderex_plugins::plugins::PluginManager::new());
        let manifest = coderex_plugins::plugins::PluginManifest {
            name: "recovering".into(),
            version: "1.0.0".into(),
            entrypoint: Some("bin/missing".into()),
            ..Default::default()
        };
        manager.load_plugin(manifest, plugin_dir.clone());
        assert!(
            manager.spawn_plugin("recovering").await.is_err(),
            "the broken entrypoint was spawned"
        );
        assert!(
            matches!(
                manager.get_plugin("recovering").map(|p| p.state),
                Some(coderex_plugins::plugins::PluginState::Error(_))
            ),
            "the failed spawn did not leave the plugin in Error"
        );

        // Fix the plugin on disk: a process that answers the JSON-RPC
        // handshake and then waits for requests.
        let script = plugin_dir.join("bin/plugin");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             while IFS= read -r line; do\n\
             \tid=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')\n\
             \tcase \"$line\" in\n\
             \t  *commands/list*|*skills/list*)\n\
             \t    printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":[]}\\n' \"$id\" ;;\n\
             \t  *)\n\
             \t    printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{}}\\n' \"$id\" ;;\n\
             \tesac\n\
             done\n",
        )
        .expect("write plugin script");
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            serde_json::json!({
                "name": "recovering",
                "version": "1.0.0",
                "entrypoint": "bin/plugin",
            })
            .to_string(),
        )
        .expect("rewrite manifest");

        let handle = StartupOrchestrator::spawn_plugin_restart_loop(
            &manager,
            std::time::Duration::from_millis(50),
        );

        // The retry pass and the spawn it triggers are both async: poll.
        let mut state = None;
        for _ in 0..200 {
            state = manager.get_plugin("recovering").map(|p| p.state);
            if matches!(state, Some(coderex_plugins::plugins::PluginState::Active)) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        handle.abort();
        let _ = manager.stop_plugin("recovering").await;
        assert!(
            matches!(state, Some(coderex_plugins::plugins::PluginState::Active)),
            "the restart loop never spawned the recovered plugin: {state:?}"
        );
    }

    #[tokio::test]
    async fn load_settings_with_nonexistent_paths_fails_open() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.cwd = "/tmp".to_string();
        let args = test_cli_args();
        // With nonexistent paths, config service returns defaults
        let result = orch.phase_load_settings(&args).await;
        // Should succeed — defaults are returned even if files don't exist
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn build_capabilities_sets_model() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings {
            model: Some("claude-opus-4-6".into()),
            permission_mode: Some("default".into()),
            ..Settings::defaults()
        });
        orch.phase_build_capabilities().await.unwrap();
        assert!(orch.ctx.model.is_some());
    }

    #[cfg(feature = "enterprise")]
    #[tokio::test]
    async fn merge_remote_no_client_resolves_immediately() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.settings = Some(Settings {
            model: Some("claude-sonnet-4-6".into()),
            permission_mode: Some("default".into()),
            max_budget_usd: Some(10.0),
            ..Settings::defaults()
        });
        orch.phase_merge_remote_settings().await.unwrap();
        // Should succeed even without remote client
    }

    #[tokio::test]
    async fn init_memory_creates_directory() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.cwd = std::env::temp_dir().to_string_lossy().to_string();
        orch.phase_init_memory().await.unwrap();
        assert!(orch.ctx.memory_ready);
    }

    #[tokio::test]
    async fn init_mcp_without_servers() {
        let mut orch = StartupOrchestrator::new();
        orch.phase_init_mcp().await.unwrap();
        assert!(!orch.ctx.mcp_ready);
    }

    #[tokio::test]
    async fn ready_phase_succeeds() {
        let orch = StartupOrchestrator::new();
        orch.phase_ready().await.unwrap();
    }

    #[test]
    fn find_project_config_returns_none_for_empty_cwd() {
        let result = StartupOrchestrator::find_project_config("/nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn resume_fails_for_missing_transcript() {
        let result =
            StartupOrchestrator::load_resume_messages("/nonexistent", "nonexistent_session");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("No transcript found"));
    }

    #[test]
    fn resume_loads_existing_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        // Write a transcript
        let transcript_dir = std::path::Path::new(&cwd).join(".coderex/transcripts");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let store = coderex_memory::transcript::TranscriptStore::new(transcript_dir);
        store
            .append(
                "sess_1",
                &coderex_core::types::Message::system("you are helpful"),
            )
            .unwrap();
        store
            .append("sess_1", &coderex_core::types::Message::user("hello"))
            .unwrap();
        store
            .append(
                "sess_1",
                &coderex_core::types::Message::assistant("hi there"),
            )
            .unwrap();

        let messages = StartupOrchestrator::load_resume_messages(&cwd, "sess_1")
            .expect("resume should load transcript messages");
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0].role, coderex_core::types::Role::User));
        assert!(matches!(
            messages[1].role,
            coderex_core::types::Role::Assistant
        ));
    }

    #[test]
    fn pull_request_context_precedes_the_initial_prompt() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.from_pr_context = Some("<pr_context>the PR</pr_context>".into());
        orch.ctx.system_prompt = Some("be terse".into());

        let prompt = orch.build_headless_prompt("fix it");

        let pr = prompt
            .find("<pr_context>the PR</pr_context>")
            .expect("the PR block must be in the prompt");
        let system = prompt.find("System: be terse").expect("system prompt");
        let user = prompt.rfind("fix it").expect("user prompt");
        assert!(
            pr < system && system < user,
            "PR context must come first: {prompt}"
        );
    }

    #[test]
    fn interactive_context_carries_files_but_headless_does_not_duplicate_them() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("notes.txt");
        std::fs::write(&file_path, "remember this").unwrap();
        let mut orch = StartupOrchestrator::new();
        orch.ctx.file = vec![file_path];

        let interactive = orch
            .interactive_context(true)
            .expect("interactive context should include the file");
        assert!(interactive.contains("remember this"));
        assert!(interactive.contains("notes.txt"));

        assert!(
            orch.interactive_context(false).is_none(),
            "headless already includes files in its prompt and must not add them twice"
        );
    }

    #[test]
    fn no_pull_request_context_leaves_the_prompt_untouched() {
        let orch = StartupOrchestrator::new();
        assert_eq!(orch.build_headless_prompt("hello"), "hello");
    }

    #[test]
    fn provider_gate_rejects_an_empty_configuration() {
        let config = ProviderConfig::default();
        assert!(
            !StartupOrchestrator::provider_config_is_usable_with(&config, &|_| None),
            "a config with no provider and no credentials must not build a client"
        );
    }

    #[test]
    fn provider_gate_accepts_an_anthropic_key() {
        let config = ProviderConfig {
            anthropic: Some(coderex_core::provider_config::ProviderEndpoint {
                api_key: "sk-test".into(),
                base_url: None,
                provider_profile: None,
                custom_headers: std::collections::BTreeMap::new(),
            }),
            ..Default::default()
        };
        assert!(StartupOrchestrator::provider_config_is_usable_with(
            &config,
            &|_| None
        ));
    }

    fn bedrock_config() -> ProviderConfig {
        ProviderConfig {
            cloud: Some(CloudEndpoint::new(ProviderKind::Bedrock, "us-east-1")),
            ..Default::default()
        }
    }

    #[test]
    fn cloud_gate_accepts_bedrock_with_aws_credentials_only() {
        let get_env = |name: &str| match name {
            "AWS_ACCESS_KEY_ID" => Some("AKIAEXAMPLE".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        };
        assert!(
            StartupOrchestrator::provider_config_is_usable_with(&bedrock_config(), &get_env),
            "AWS SigV4 credentials are sufficient for Bedrock; a first-party key is not required"
        );
    }

    #[test]
    fn cloud_gate_rejects_a_cloud_selector_without_credentials() {
        assert!(!StartupOrchestrator::provider_config_is_usable_with(
            &bedrock_config(),
            &|_| None
        ));
    }

    #[test]
    fn cloud_client_is_constructed_without_a_first_party_key() {
        let get_env = |name: &str| match name {
            "AWS_ACCESS_KEY_ID" => Some("AKIAEXAMPLE".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        };
        let client = StartupOrchestrator::api_client_from_provider_config(
            bedrock_config(),
            None,
            None,
            None,
            get_env,
        );
        assert!(
            client.is_some(),
            "a valid cloud deployment must not be blocked by the CLI provider gate"
        );
    }

    #[test]
    fn effort_accepts_the_three_levels_and_rejects_anything_else() {
        for level in ["low", "medium", "high", "HIGH"] {
            assert!(
                StartupOrchestrator::validate_effort(Some(level)).is_ok(),
                "{level} should be accepted"
            );
        }
        assert!(StartupOrchestrator::validate_effort(None).is_ok());
        let error = StartupOrchestrator::validate_effort(Some("maximum"))
            .expect_err("an unknown level must not be accepted silently");
        assert!(
            error
                .to_string()
                .contains("expected one of low, medium, high"),
            "{error}"
        );
    }

    #[test]
    fn apply_effort_wraps_the_client_and_stamps_the_context() {
        use coderex_core::api_client::{ApiResponse, RequestContext, StreamEvent};
        use coderex_core::types::ModelId as CoreModelId;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        #[derive(Debug, Default)]
        struct SpyClient {
            seen_effort: std::sync::Arc<std::sync::Mutex<Option<String>>>,
            context_calls: std::sync::Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiClient for SpyClient {
            fn clone_box(&self) -> Box<dyn ApiClient> {
                Box::new(Self {
                    seen_effort: std::sync::Arc::clone(&self.seen_effort),
                    context_calls: std::sync::Arc::clone(&self.context_calls),
                })
            }

            fn with_request_context(&self, context: RequestContext) -> Box<dyn ApiClient> {
                self.context_calls.fetch_add(1, AtomicOrdering::SeqCst);
                *self.seen_effort.lock().unwrap() = context
                    .reasoning_effort()
                    .map(|effort| effort.as_str().to_string());
                Box::new(Self {
                    seen_effort: std::sync::Arc::clone(&self.seen_effort),
                    context_calls: std::sync::Arc::clone(&self.context_calls),
                })
            }

            async fn create_message(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &CoreModelId,
                _max_tokens: u32,
                _tools: &[coderex_core::tool_executor::ToolInfo],
            ) -> Result<ApiResponse, coderex_core::api_client::ApiError> {
                Err(coderex_core::api_client::ApiError::Auth("spy".into()))
            }

            async fn create_message_streaming(
                &self,
                _system: &str,
                _messages: &[serde_json::Value],
                _model: &CoreModelId,
                _max_tokens: u32,
                _tools: &[coderex_core::tool_executor::ToolInfo],
            ) -> Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send + 'static>> {
                Box::pin(futures::stream::empty())
            }
        }

        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let spy = SpyClient {
            seen_effort: std::sync::Arc::clone(&seen),
            context_calls: std::sync::Arc::clone(&calls),
        };

        let wrapped = StartupOrchestrator::apply_effort(Box::new(spy), Some("high"));
        let _ = wrapped.with_request_context(RequestContext::default());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(seen.lock().unwrap().as_deref(), Some("high"));

        // No effort: the client is passed through untouched, so the context
        // reaches it without a reasoning-effort hint.
        let plain = StartupOrchestrator::apply_effort(
            Box::new(SpyClient {
                seen_effort: std::sync::Arc::clone(&seen),
                context_calls: std::sync::Arc::clone(&calls),
            }),
            None,
        );
        let _ = plain.with_request_context(RequestContext::default());
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(seen.lock().unwrap().as_deref(), None);
    }

    #[test]
    fn append_system_prompt_is_labelled_as_additive() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.append_system_prompt = Some("be terse".into());
        let prompt = orch.build_headless_prompt("hello");
        assert!(prompt.contains("Additional system instructions: be terse"));
        assert!(!prompt.contains("System: be terse"));
    }

    /// `fast_mode` changes the *default* session model: with nothing explicitly
    /// configured it resolves through `ModelProfile::Lightweight`, and with the
    /// flag off the default is sonnet. An explicit model always wins.
    #[test]
    fn fast_mode_selects_the_lightweight_default_only_when_nothing_is_explicit() {
        assert_eq!(
            StartupOrchestrator::resolve_session_model(None, false),
            ModelId::sonnet(),
            "without fast_mode the default session model is sonnet"
        );
        assert_eq!(
            StartupOrchestrator::resolve_session_model(None, true),
            ModelProfile::Lightweight.resolve_runtime(),
            "fast_mode resolves the default through the lightweight profile"
        );

        let explicit = ModelId::sonnet();
        assert_eq!(
            StartupOrchestrator::resolve_session_model(Some(explicit.clone()), true),
            explicit,
            "an explicit model wins over fast_mode"
        );
    }

    #[test]
    fn headless_prompt_carries_both_prompt_flags() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.system_prompt = Some("replace the base".into());
        orch.ctx.append_system_prompt = Some("add this".into());

        let prompt = orch.build_headless_prompt("hello");

        assert!(
            prompt.contains("System: replace the base"),
            "the replacement prompt must be a labelled block: {prompt}"
        );
        assert!(
            prompt.contains("Additional system instructions: add this"),
            "the append text must be a labelled block: {prompt}"
        );
        let system = prompt.find("System: replace the base").unwrap();
        let append = prompt
            .find("Additional system instructions: add this")
            .unwrap();
        let user = prompt.rfind("hello").unwrap();
        assert!(
            system < append && append < user,
            "prompt blocks must precede the user prompt: {prompt}"
        );
    }

    #[test]
    fn interactive_mode_applies_both_prompt_flags_independently() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.system_prompt = Some("replace the base".into());
        orch.ctx.append_system_prompt = Some("add this".into());

        let mut repl = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_repl(&mut repl, true);

        let (replacement, appended) = repl.system_prompt_effects_for_test();
        assert_eq!(replacement, Some("replace the base"));
        assert_eq!(appended, Some("add this"));
    }

    #[test]
    fn interactive_mode_applies_an_append_prompt_on_its_own() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.append_system_prompt = Some("only additive".into());

        let mut repl = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_repl(&mut repl, true);

        let (replacement, appended) = repl.system_prompt_effects_for_test();
        assert!(
            replacement.is_none(),
            "an append flag alone must not replace the base prompt"
        );
        assert_eq!(appended, Some("only additive"));
    }

    #[test]
    fn fallback_is_used_only_for_model_unavailability() {
        let primary = ModelId("claude-opus-4-6".into());
        let fallback = ModelId("claude-haiku-4-5-20251001".into());
        let choose = coderex_core::engine::fallback_after_model_error;

        assert_eq!(
            choose(
                &primary,
                Some(&fallback),
                "404 model_not_found: claude-opus-4-6"
            )
            .map(|model| model.0),
            Some(fallback.0.clone())
        );
        assert!(
            choose(&primary, Some(&fallback), "connection reset by peer").is_none(),
            "a transport error is not evidence the model is unavailable"
        );
        assert!(choose(&primary, Some(&primary), "model_not_found").is_none());
        assert!(choose(&primary, None, "model_not_found").is_none());
    }

    /// Interactive mode accepts `--fallback-model` and wires it into the
    /// session instead of refusing it.
    #[test]
    fn interactive_accepts_and_wires_the_fallback_model() {
        let mut orch = StartupOrchestrator::new();
        orch.ctx.fallback_model = Some(ModelId("claude-haiku-4-5-20251001".into()));

        let mut interactive = crate::repl::ReplLoop::new("/tmp".into(), ModelId::sonnet());
        orch.configure_repl(&mut interactive, true);
        assert_eq!(
            interactive.fallback_model().map(|model| model.0.as_str()),
            Some("claude-haiku-4-5-20251001")
        );
    }

    #[test]
    fn telemetry_settings_override_environment_channels() {
        let settings = Settings {
            telemetry: Some(coderex_config::settings::TelemetrySubConfig {
                otlp_endpoint: Some("https://collector.example/v1/logs".into()),
                otlp_headers: Some("Authorization=Bearer token,x-team=core".into()),
                datadog_api_key: Some("dd-key".into()),
                datadog_site: Some("datadoghq.eu".into()),
                datadog_endpoint: None,
                ..Default::default()
            }),
            ..Settings::defaults()
        };
        let channels = StartupOrchestrator::apply_telemetry_channel_settings(
            coderex_telemetry::ChannelConfig::default(),
            &settings,
        );
        assert_eq!(
            channels.otlp_endpoint.as_deref(),
            Some("https://collector.example/v1/logs")
        );
        assert_eq!(
            channels.otlp_headers,
            vec![
                ("Authorization".to_string(), "Bearer token".to_string()),
                ("x-team".to_string(), "core".to_string()),
            ]
        );
        assert_eq!(channels.datadog_api_key.as_deref(), Some("dd-key"));
        assert_eq!(channels.datadog_site, "datadoghq.eu");
    }

    /// The strict setting reaches the payload the uploader would send.
    ///
    /// Settings and the uploader each had a test; the seam between them — does
    /// `telemetry.strict` actually change what leaves the machine — did not.
    /// Both payloads come from the same uploader constructor a session uses.
    #[test]
    fn the_strict_setting_changes_the_uploaded_crash_payload() {
        let crash = coderex_telemetry::PendingCrash {
            id: 1,
            panic_info: r#"{"panic_type":"test","panic_location":"src/lib.rs:1"}"#.into(),
            backtrace: None,
            created_at: "2026-10-29T00:00:00Z".into(),
            panic_template: None,
            panic_fields: None,
            panic_message_len: Some(4),
            panic_message_sha256: Some("a".repeat(64)),
            backtrace_symbols: Some(
                serde_json::json!(["core::panicking::panic", "test::boom"]).to_string(),
            ),
            backtrace_frame_count: Some(2),
            backtrace_symbols_len: Some(2),
            backtrace_symbols_sha256: Some("b".repeat(64)),
        };

        let payload_for = |strict: bool| {
            let settings = Settings {
                telemetry: Some(coderex_config::settings::TelemetrySubConfig {
                    strict: Some(strict),
                    ..Default::default()
                }),
                ..Settings::defaults()
            };
            let config = StartupOrchestrator::telemetry_config_from_settings(&settings);
            assert_eq!(
                config.strict, strict,
                "the setting did not reach the config"
            );
            let dir = tempfile::tempdir().unwrap();
            let uploader =
                coderex_telemetry::TelemetryUploader::new(dir.path().join("telemetry.db"), config);
            uploader.preview_crash_payload(&crash)
        };

        let relaxed = payload_for(false);
        assert!(
            relaxed.get("backtrace_frames").is_some(),
            "a non-strict payload should carry the symbol frames: {relaxed}"
        );

        let strict = payload_for(true);
        assert!(
            strict.get("backtrace_frames").is_none(),
            "strict mode still uploaded symbol frames: {strict}"
        );
        assert!(
            strict.get("panic_info").is_none(),
            "strict mode still uploaded free-form text: {strict}"
        );
    }

    #[tokio::test]
    async fn uploader_task_is_spawned_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let disabled_store =
            coderex_telemetry::TelemetryStore::open(&dir.path().join("off.db")).unwrap();
        let (_handle, uploader) = coderex_telemetry::TelemetryHandle::new(
            &disabled_store,
            Some(coderex_telemetry::TelemetryConfig {
                enabled: false,
                ..Default::default()
            }),
        );
        assert!(
            StartupOrchestrator::spawn_telemetry_uploader(uploader).is_none(),
            "an opted-out session must not start an uploader task"
        );

        let enabled_store =
            coderex_telemetry::TelemetryStore::open(&dir.path().join("on.db")).unwrap();
        let (_handle, uploader) = coderex_telemetry::TelemetryHandle::new(
            &enabled_store,
            Some(coderex_telemetry::TelemetryConfig {
                enabled: true,
                upload_interval_secs: 3600,
                ..Default::default()
            }),
        );
        let guard = StartupOrchestrator::spawn_telemetry_uploader(uploader)
            .expect("an opted-in session must start an uploader task");
        assert!(!guard.is_finished(), "the uploader task should be running");
        drop(guard);
    }

    #[test]
    fn session_guard_records_start_and_end_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("telemetry.db");
        let store = coderex_telemetry::TelemetryStore::open(&db_path).unwrap();
        let (handle, _uploader) = coderex_telemetry::TelemetryHandle::new(&store, None);
        {
            let _session = TelemetrySessionGuard::start(handle);
        }
        let store = coderex_telemetry::TelemetryStore::open(&db_path).unwrap();
        assert_eq!(
            store.event_count().unwrap(),
            2,
            "a clean session must record both a start and an end event"
        );
    }

    #[test]
    fn configured_marketplaces_include_settings_and_drop_duplicates() {
        let configured = vec![
            coderex_config::MarketplaceSetting {
                name: "internal".into(),
                url: "https://plugins.example".into(),
            },
            coderex_config::MarketplaceSetting {
                name: "internal".into(),
                url: "https://plugins.example".into(),
            },
        ];
        let entries = StartupOrchestrator::configured_marketplaces(&configured);
        assert_eq!(
            entries
                .iter()
                .filter(|(name, _)| name == "internal")
                .count(),
            1,
            "the same marketplace must be registered once"
        );
        assert!(entries.contains(&(
            "internal".to_string(),
            "https://plugins.example".to_string()
        )));
    }

    #[test]
    fn try_create_api_client_builds_a_router_for_a_configured_key() {
        // Direct process-environment mutation is blocked by `-D unsafe-code`,
        // so exercise the deterministic constructor instead of guessing what
        // the ambient environment contains.
        let client = StartupOrchestrator::api_client_from_provider_config(
            ProviderConfig {
                anthropic: Some(coderex_core::provider_config::ProviderEndpoint {
                    api_key: "sk-test".into(),
                    base_url: None,
                    provider_profile: None,
                    custom_headers: std::collections::BTreeMap::new(),
                }),
                ..Default::default()
            },
            None,
            None,
            None,
            |_| None,
        );
        assert!(
            client.is_some(),
            "a configured provider endpoint must produce a client"
        );
    }
}
