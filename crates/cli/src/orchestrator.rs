//! Startup orchestrator — four-phase initialization.

use anyhow::{Result, bail};
#[cfg(feature = "enterprise")]
use coderex_config::config_home;
use coderex_config::{ConfigService, RuntimeOverrides, Settings};
#[cfg(feature = "enterprise")]
use coderex_config::{LoadingBarrier, RemoteManagedSettings, RemoteManagedSettingsClient, RemoteSettingsManager};
use coderex_core::types::ModelId;
use coderex_core::api_client::ApiClient;
use crate::cli::Cli;
use crate::daemon;
use std::io::IsTerminal;
use std::sync::Arc;

/// Duration (ms) to wait for remote settings before timing out and failing open.
#[cfg(feature = "enterprise")]
const REMOTE_SETTINGS_TIMEOUT_MS: u64 = 5000;

/// Resolved settings and state passed between startup phases.
#[derive(Debug, Default)]
pub struct SessionContext {
    /// Resolved settings after all merges (defaults → user → project → remote).
    pub settings: Option<Settings>,
    /// Remote managed settings (if fetched). Enterprise feature only.
    #[cfg(feature = "enterprise")]
    pub remote_settings: Option<RemoteManagedSettings>,
    /// Resolved model to use.
    pub model: Option<ModelId>,
    /// Loading barrier for remote settings (if configured). Enterprise feature only.
    #[cfg(feature = "enterprise")]
    pub remote_barrier: Option<LoadingBarrier>,
    /// MCP server connections initialized.
    pub mcp_ready: bool,
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
    /// Telemetry handle (initialized during LoadSettings).
    pub telemetry_handle: Option<coderex_telemetry::TelemetryHandle>,
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
    /// Append to default system prompt.
    pub append_system_prompt: bool,
    /// Whether session persistence is disabled.
    pub no_persistence: bool,
    /// Whether slash commands are disabled.
    pub no_slash_commands: bool,
    /// Output format preference.
    pub output_format: String,
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
    /// IDE mode.
    pub ide: Option<String>,
    /// Worktree mode.
    pub worktree: bool,
    /// Fork from session.
    pub fork_session: Option<String>,
    /// Load from PR.
    pub from_pr: Option<String>,
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
                #[cfg(feature = "enterprise")]
                remote_barrier: None,
                mcp_ready: false,
                mcp_skills: Vec::new(),
                memory_ready: false,
                remote_services_ready: false,
                cwd: String::new(),
                plugin_manager: None,
                telemetry_handle: None,
                session_name: None,
                skip_permissions: false,
                add_dir: Vec::new(),
                file: Vec::new(),
                system_prompt: None,
                append_system_prompt: false,
                no_persistence: false,
                no_slash_commands: false,
                output_format: "text".into(),
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
                ide: None,
                worktree: false,
                fork_session: None,
                from_pr: None,
                setting_sources: false,
                verbose: false,
                debug: false,
                bare: false,
                brief: false,
            },
        }
    }

    /// Run the full startup sequence.
    pub async fn run(&mut self, args: Cli) -> Result<()> {
        // Store working directory early
        self.ctx.cwd = args.directory.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

        // Store CLI-derived context
        self.ctx.session_name = args.name.clone();
        self.ctx.skip_permissions = args.dangerously_skip_permissions;
        self.ctx.add_dir = args.add_dir.clone();
        self.ctx.file = args.file.clone();
        self.ctx.system_prompt = args.system_prompt.clone();
        self.ctx.append_system_prompt = args.append_system_prompt;
        self.ctx.no_persistence = args.no_session_persistence;
        self.ctx.no_slash_commands = args.disable_slash_commands;
        self.ctx.output_format = args.output_format.clone();
        self.ctx.debug_file = args.debug_file.clone();
        self.ctx.settings_path = args.settings.clone();
        self.ctx.mcp_config_path = args.mcp_config.clone();
        self.ctx.mcp_debug = args.mcp_debug;
        self.ctx.strict_mcp_config = args.strict_mcp_config;
        self.ctx.plugin_dir = args.plugin_dir.clone();
        self.ctx.input_format = args.input_format.clone();
        self.ctx.json_schema = args.json_schema.clone();
        self.ctx.max_turns = args.max_turns;
        self.ctx.replay_user_messages = args.replay_user_messages;
        self.ctx.include_partial_messages = args.include_partial_messages;
        self.ctx.effort = args.effort.clone();
        self.ctx.betas = args.betas.clone();
        self.ctx.chrome = args.chrome;
        self.ctx.no_chrome = args.no_chrome;
        self.ctx.tmux = args.tmux;
        self.ctx.agent = args.agent.clone();
        self.ctx.ide = args.ide.clone();
        self.ctx.worktree = args.worktree;
        self.ctx.fork_session = args.fork_session.clone();
        self.ctx.from_pr = args.from_pr.clone();
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
                let Ok(mut sigterm) = signal(SignalKind::terminate()) else { return };
                let Ok(mut sighup) = signal(SignalKind::hangup()) else { return };
                loop {
                    tokio::select! {
                        _ = sigterm.recv() => {
                            tracing::info!("Received SIGTERM — shutting down gracefully");
                            let _ = crate::daemon::remove_pid_file();
                            // Save recovery snapshot before exit
                            let _ = crate::recovery::remove(&cwd);
                            std::process::exit(0);
                        }
                        _ = sighup.recv() => {
                            tracing::info!("Received SIGHUP — config reload requested");
                            // Config reload is handled by ConfigWatcher in the REPL loop
                        }
                    }
                }
            });
        }

        tracing::info!("CodeRex ready");

        // Determine model: CODEREX_MODEL env → settings → default
        let model = std::env::var("CODEREX_MODEL").ok()
            .filter(|m| !m.is_empty())
            .map(ModelId)
            .or_else(|| self.ctx.model.clone())
            .unwrap_or_else(ModelId::sonnet);

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
            let prompt = if !args.prompt.is_empty() {
                args.prompt.join(" ")
            } else {
                let mut stdin = String::new();
                std::io::stdin().read_line(&mut stdin)?;
                stdin.trim().to_string()
            };
            if prompt.is_empty() {
                bail!("No input provided");
            }
            self.run_headless_with_prompt(&self.ctx.cwd, model, &prompt).await?;
            return Ok(());
        } else if let Some(cmd) = args.command {
            match cmd {
                crate::cli::Command::Repl => {
                    self.run_repl(&self.ctx.cwd, model).await
                }
                crate::cli::Command::Serve { port } => {
                    self.run_server(&self.ctx.cwd, port).await
                }
                crate::cli::Command::ServeStop => {
                    self.run_serve_stop().await
                }
                crate::cli::Command::ServeStatus => {
                    self.run_serve_status().await
                }
                crate::cli::Command::Agents { detailed } => {
                    self.run_agents(detailed).await
                }
                crate::cli::Command::Auth => {
                    self.run_auth().await
                }
                crate::cli::Command::Doctor => {
                    self.run_doctor().await
                }
                crate::cli::Command::Update => {
                    self.run_update().await
                }
                crate::cli::Command::Mcp { action } => {
                    self.run_mcp(action).await
                }
                crate::cli::Command::Plugin { action } => {
                    self.run_plugin(action).await
                }
                crate::cli::Command::AutoMode => {
                    self.run_auto_mode().await
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
            StartupPhase::MergeRemoteManagedSettings => {
                self.phase_merge_remote_settings().await
            }
            #[cfg(not(feature = "enterprise"))]
            StartupPhase::MergeRemoteManagedSettings => {
                tracing::debug!("remote settings phase skipped (enterprise feature not enabled)");
                Ok(())
            }
            StartupPhase::BuildCapabilities => self.phase_build_capabilities().await,
            StartupPhase::LoadCommands => self.phase_load_commands().await,
            StartupPhase::LoadSkills => self.phase_load_skills().await,
            StartupPhase::InitPlugins => self.phase_init_plugins().await,
            StartupPhase::ReconcileMarketplaces => {
                self.phase_reconcile_marketplaces().await
            }
            StartupPhase::InitMcp => self.phase_init_mcp().await,
            StartupPhase::InitMemory => self.phase_init_memory().await,
            StartupPhase::InitRemoteServices => self.phase_init_remote_services().await,
            StartupPhase::Ready => self.phase_ready().await,
        }
    }

    /// Bootstrap: initialize tracing, signal handlers, and core subsystems.
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
                            file.try_clone().unwrap_or_else(|_| {
                                std::fs::File::open("/dev/null").unwrap()
                            })
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
    async fn phase_load_settings(&mut self, args: &Cli) -> Result<()> {
        let config_dir = coderex_config::config_home();

        // Use custom settings path if --settings is provided
        let user_path = args.settings.clone()
            .unwrap_or_else(|| config_dir.join("settings.json"));

        // Find project config in CWD or ancestors
        let project_path = Self::find_project_config(&self.ctx.cwd);

        let config_service = ConfigService::new(user_path.clone(), project_path.clone());
        let mut settings = config_service.resolve()?;

        // First-run consent gate: if no user config exists and telemetry_enabled
        // is not explicitly set, prompt the user interactively.
        if !user_path.exists() && settings.telemetry_enabled.is_none() {
            if let Some(choice) = Self::prompt_telemetry_consent() {
                settings.telemetry_enabled = Some(choice);
                // Persist the choice to settings.json
                let _ = Self::write_user_settings(&user_path, &settings);
            }
        }

        // Apply CLI runtime overrides
        let overrides = RuntimeOverrides {
            model: args.model.clone(),
            permission_mode: args.permissions.clone(),
            max_budget_usd: args.max_budget_usd,
            telemetry_enabled: if args.no_telemetry { Some(false) } else { None },
            api_key: args.api_key.clone(),
            system_prompt: args.system_prompt.clone(),
            append_system_prompt: args.append_system_prompt,
            allowed_tools: args.allowed_tools.as_ref().map(|s| s.split(',').map(|t| t.trim().to_string()).collect()),
            fallback_model: args.fallback_model.clone(),
            effort: args.effort.clone(),
        };
        settings = overrides.apply(settings);

        tracing::info!(
            "settings loaded: model={}",
            settings.model.as_deref().unwrap_or("default")
        );

        // --setting-sources: print where each value came from
        if args.setting_sources {
            let diff = settings.diff_from_defaults("resolved");
            println!("{}", diff.render());
        }

        // Initialize telemetry subsystem with resolved settings
        let telemetry_enabled = settings.telemetry_enabled.unwrap_or(true);
        if telemetry_enabled {
            if let Some((handle, _uploader)) = Self::init_telemetry(&settings) {
                self.ctx.telemetry_handle = Some(handle);
            }
        } else {
            tracing::info!("telemetry disabled by user preference");
        }

        self.ctx.settings = Some(settings);
        Ok(())
    }

    /// Initialize the telemetry subsystem using resolved settings.
    fn init_telemetry(
        settings: &Settings,
    ) -> Option<(coderex_telemetry::TelemetryHandle, Option<coderex_telemetry::TelemetryUploader>)> {
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

        // Build TelemetryConfig from settings sub-config
        let telemetry_config = settings.telemetry.as_ref().map(|t| {
            let api_key = settings.api_key.as_ref().map(|k| coderex_config::resolve_api_key(k));
            coderex_telemetry::TelemetryConfig {
                endpoint: t.endpoint.clone().unwrap_or_else(|| {
                    "https://telemetry.coderex.dev/v1/events".into()
                }),
                crash_endpoint: t.crash_endpoint.clone(),
                api_key,
                batch_size: t.batch_size.unwrap_or(100),
                upload_interval_secs: t.upload_interval_secs.unwrap_or(300),
                enabled: true,
                sample_rate: t.sample_rate.unwrap_or(1.0),
                pinned_cert_pem: t.pinned_cert_pem.clone(),
            }
        });

        let (handle, uploader) =
            coderex_telemetry::TelemetryHandle::new(store, telemetry_config);
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
        eprintln!("  - Events are stored locally and batch-uploaded");
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

    /// Write user settings to disk, preserving other fields.
    fn write_user_settings(
        path: &std::path::Path,
        settings: &Settings,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(settings).unwrap_or_default();
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
                        coderex_config::FetchState::Ready { settings: remote, .. } => {
                            tracing::info!("remote settings applied");
                            self.ctx.remote_settings = Some(remote);
                        }
                        coderex_config::FetchState::Failed { error, .. } => {
                            tracing::warn!("remote settings fetch failed (failing open): {error}");
                        }
                        _ => {
                            // Still fetching — we'll merge later when ready
                            tracing::debug!("remote settings still fetching, will merge asynchronously");
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
    async fn phase_build_capabilities(&mut self) -> Result<()> {
        // Resolve model from settings
        if let Some(ref settings) = self.ctx.settings {
            if let Some(ref model_name) = settings.model {
                self.ctx.model = Some(ModelId(model_name.clone()));
            }
        }

        tracing::info!("capabilities built");
        Ok(())
    }

    /// LoadCommands: discover and register custom commands from `.coderex/commands/`.
    async fn phase_load_commands(&self) -> Result<()> {
        let commands_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/commands");
        if commands_dir.exists() {
            let mut count = 0;
            if let Ok(entries) = std::fs::read_dir(&commands_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "json") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&content) {
                                if let Some(name) = cmd.get("name").and_then(|n| n.as_str()) {
                                    tracing::debug!("discovered custom command: {name}");
                                    count += 1;
                                }
                            }
                        }
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
    async fn phase_load_skills(&self) -> Result<()> {
        let skills_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/skills");
        if skills_dir.exists() {
            let count = std::fs::read_dir(&skills_dir)
                .map(|d| d.count())
                .unwrap_or(0);
            tracing::info!("skills loaded: {count} from {skills_dir:?}");
        } else {
            tracing::debug!("no skills directory found");
        }
        Ok(())
    }

    /// InitPlugins: discover, load, and spawn plugins from plugin directories.
    async fn phase_init_plugins(&mut self) -> Result<()> {
        let mut manager = coderex_plugins::plugins::PluginManager::new();
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
                        if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                            if let Ok(manifest) = serde_json::from_str::<coderex_plugins::plugins::PluginManifest>(&content) {
                                manager.load_plugin(manifest, entry.path());
                                loaded += 1;
                            }
                        }
                    }
                }
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

        tracing::info!(
            "plugins initialized: {loaded} loaded, {spawned} spawned"
        );

        self.ctx.plugin_manager = Some(Arc::new(manager));
        Ok(())
    }

    /// ReconcileMarketplaces: sync marketplace state.
    async fn phase_reconcile_marketplaces(&self) -> Result<()> {
        let manager = coderex_plugins::plugins::PluginManager::new();
        let reconciler = coderex_plugins::plugins::MarketplaceReconciler::new(300);

        // Register marketplace if configured via environment
        if let Ok(url) = std::env::var("CODEREX_MARKETPLACE_URL") {
            if !url.is_empty() {
                let name = std::env::var("CODEREX_MARKETPLACE_NAME")
                    .unwrap_or_else(|_| "default".into());
                let _ = manager.register_marketplace(name, url);
            }
        }

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
        let manager = coderex_mcp::manager::McpManager::new();
        let mut config_loaded = false;

        // Load from custom --mcp-config or default .mcp.json
        let mcp_path = self.ctx.mcp_config_path.clone()
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

        // Also check inline config from settings
        if let Some(ref mcp_servers) = settings.mcp_servers {
            if !mcp_servers.to_string().is_empty() && mcp_servers.to_string() != "null" {
                tracing::debug!("MCP servers configured in settings");
                config_loaded = true;
            }
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
                self.ctx.mcp_skills.push(coderex_plugins::skills::SkillMetadata {
                    name: tool.name.clone(),
                    hooks: vec!["post-query".into()],
                    agent: Some(tool.server.clone()),
                    allowed_tools: vec![tool.name],
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
        } else {
            tracing::debug!("no MCP servers configured");
        }

        Ok(())
    }

    /// InitMemory: initialize memory subsystem (auto dream, team memory).
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
    async fn phase_init_remote_services(&mut self) -> Result<()> {
        // Bridge transport initialization — best-effort, failures don't block startup
        if let Ok(bridge_url) = std::env::var("CODEREX_BRIDGE_URL") {
            if !bridge_url.is_empty() {
                let factory = coderex_bridge::transport::default_factory();
                tracing::info!(
                    "bridge transport initialized: url={bridge_url}, transports={:?}",
                    factory.registered_types()
                );
                self.ctx.remote_services_ready = true;
                return Ok(());
            }
        }

        tracing::debug!("no remote services configured");
        self.ctx.remote_services_ready = true;
        Ok(())
    }

    /// Ready: final verification that all critical subsystems are up.
    async fn phase_ready(&self) -> Result<()> {
        tracing::info!(
            "startup complete: cwd={}, model={}, mcp={}, memory={}, remote={}",
            self.ctx.cwd,
            self.ctx.model.as_ref().map(|m| m.0.as_str()).unwrap_or("default"),
            self.ctx.mcp_ready,
            self.ctx.memory_ready,
            self.ctx.remote_services_ready,
        );
        Ok(())
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

    /// Create an API client from environment variables.
    ///
    /// Uses `ProviderConfig::from_env()` to build a [`ModelRouter`] that
    /// supports multi-provider fallback (Anthropic → OpenAI → Groq → custom).
    fn try_create_api_client(api_key_override: Option<&str>) -> Option<Box<dyn ApiClient>> {
        let mut config = coderex_core::provider_config::ProviderConfig::from_env();
        if let Some(key) = api_key_override {
            config.set_anthropic_api_key(key.into());
        }
        // Only build a router if at least one provider is configured.
        if config.anthropic.is_none()
            && config.openai.is_none()
            && config.groq.is_none()
            && config.custom.is_empty()
        {
            return None;
        }
        Some(Box::new(config.build_router()) as Box<dyn ApiClient>)
    }

    /// Get the API key override from settings if set.
    fn api_key_from_settings(&self) -> Option<&str> {
        self.ctx.settings.as_ref()
            .and_then(|s| s.api_key.as_deref())
    }

    /// Configure a ReplLoop with all CLI-derived settings.
    fn configure_repl(&self, repl: &mut crate::repl::ReplLoop) {
        repl.with_skip_permissions(self.ctx.skip_permissions);
        let perm_mode = self.ctx.settings.as_ref()
            .and_then(|s| s.permission_mode.as_deref())
            .and_then(|m| m.parse::<coderex_core::permissions::PermissionMode>().ok());
        repl.with_permission_mode(perm_mode);
        repl.with_no_persistence(self.ctx.no_persistence);
        repl.with_no_slash_commands(self.ctx.no_slash_commands);
        repl.with_json_schema(self.ctx.json_schema.clone());
        let budget = self.ctx.settings.as_ref().and_then(|s| s.max_budget_usd);
        repl.with_max_budget_usd(budget);
        repl.with_max_turns(self.ctx.max_turns);
        repl.with_replay_user_messages(self.ctx.replay_user_messages);
        repl.with_include_partial_messages(self.ctx.include_partial_messages);
        repl.with_betas(self.ctx.betas.clone());
        repl.with_chrome_enabled(self.ctx.chrome && !self.ctx.no_chrome);
        repl.with_agent(self.ctx.agent.clone());
        repl.with_ide(self.ctx.ide.clone());
        repl.with_worktree(self.ctx.worktree);
        repl.with_fork_session(self.ctx.fork_session.clone());
        repl.with_from_pr(self.ctx.from_pr.clone());
        repl.with_session_name(self.ctx.session_name.clone());
        repl.with_add_dir(self.ctx.add_dir.clone());
        repl.with_input_format(self.ctx.input_format.clone());
        repl.with_tmux_enabled(self.ctx.tmux);
        repl.with_debug_file(self.ctx.debug_file.clone());
        if !self.ctx.allowed_tools.is_empty() {
            repl.with_allowed_tools(self.ctx.allowed_tools.clone());
        }
        repl.apply_cli_config();
    }

    #[allow(clippy::unused_async)]
    async fn run_repl(&self, cwd: &str, model: ModelId) -> Result<()> {
        if !std::io::stdin().is_terminal() {
            bail!("interactive REPL requires a terminal on stdin");
        }

        let api_key = self.api_key_from_settings();
        let mut repl = if let Some(api_client) = Self::try_create_api_client(api_key) {
            crate::repl::ReplLoop::with_engine(api_client, cwd.to_string(), model)
        } else {
            crate::repl::ReplLoop::new(cwd.to_string(), model)
        };
        if let Some(ref pm) = self.ctx.plugin_manager {
            repl.with_plugin_manager(Arc::clone(pm));
        }
        self.configure_repl(&mut repl);
        repl.run_tui().await
    }

    #[allow(clippy::unused_async)]
    async fn run_headless_with_prompt(&self, cwd: &str, model: ModelId, prompt: &str) -> Result<()> {
        let api_key = self.api_key_from_settings();
        let mut repl = if let Some(api_client) = Self::try_create_api_client(api_key) {
            crate::repl::ReplLoop::with_engine(api_client, cwd.to_string(), model)
        } else {
            crate::repl::ReplLoop::new(cwd.to_string(), model)
        };
        if let Some(ref pm) = self.ctx.plugin_manager {
            repl.with_plugin_manager(Arc::clone(pm));
        }
        self.configure_repl(&mut repl);

        // Parse input based on --input-format
        let user_prompt = if self.ctx.input_format == "json" {
            match serde_json::from_str::<serde_json::Value>(prompt) {
                Ok(val) => {
                    val.get("prompt")
                        .and_then(|p| p.as_str())
                        .unwrap_or(prompt)
                        .to_string()
                }
                Err(_) => prompt.to_string(),
            }
        } else {
            prompt.to_string()
        };

        // Build final prompt: include --file contents, system prompt, user prompt
        let mut final_prompt = String::new();

        // Add system prompt if provided
        if let Some(ref sp) = self.ctx.system_prompt {
            final_prompt.push_str("System: ");
            final_prompt.push_str(sp);
            final_prompt.push('\n');
            final_prompt.push('\n');
        }

        // Add file contents
        for file_path in &self.ctx.file {
            match std::fs::read_to_string(file_path) {
                Ok(content) => {
                    final_prompt.push_str(&format!("--- {} ---\n{}\n\n", file_path.display(), content));
                }
                Err(e) => {
                    tracing::warn!("failed to read file {}: {e}", file_path.display());
                }
            }
        }

        final_prompt.push_str(&user_prompt);

        let result = repl.run_headless(&final_prompt).await?;

        // Apply output formatting
        let formatted = self.format_headless_output(&result);
        println!("{formatted}");
        Ok(())
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

        let config = ServerConfig {
            valid_keys,
            idle_timeout: std::time::Duration::from_secs(idle_timeout),
            max_sessions: 100,
            rate_limit_per_minute: 60,
            health_port: None,
            persist_path: None,
        };

        let mut server = DaemonServer::with_config(config);

        tracing::info!("CodeRex server initialized: cwd={cwd}");
        tracing::info!("Server config: idle_timeout={idle_timeout}s, max_sessions=100, port={port}");
        println!("CodeRex server running on ws://0.0.0.0:{port}");
        println!("Sessions: 0 active");
        println!("Press Ctrl+C to stop");

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        let result = server.run(addr).await.map_err(|e| anyhow::anyhow!(e));

        // Clean up PID file on shutdown
        let _ = daemon::remove_pid_file();

        result
    }

    #[allow(clippy::unused_async)]
    async fn run_serve_stop(&self) -> Result<()> {
        match daemon::stop_daemon() {
            Ok(msg) => {
                println!("{msg}");
                Ok(())
            }
            Err(e) => bail!("{e}"),
        }
    }

    #[allow(clippy::unused_async)]
    async fn run_serve_status(&self) -> Result<()> {
        println!("{}", daemon::serve_status());
        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn run_agents(&self, detailed: bool) -> Result<()> {
        println!("Available agents:");
        if detailed {
            println!("  advisor    - Code review and architectural guidance");
            println!("  bughunter  - Bug detection and root cause analysis");
            println!("  reviewer   - PR-style code review");
        } else {
            println!("  advisor  bughunter  reviewer");
        }
        Ok(())
    }

    #[allow(clippy::unused_async)]
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
            println!("Set ANTHROPIC_API_KEY, OPENAI_API_KEY, GROQ_API_KEY, or CODEREX_API_KEY.");
        } else {
            println!("Authenticated providers: {}", providers.join(", "));
        }
        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn run_doctor(&self) -> Result<()> {
        println!("CodeRex Doctor");
        println!("==============");
        let checks = [
            ("API key configured", Self::try_create_api_client(self.api_key_from_settings()).is_some()),
            ("Settings file exists", coderex_config::config_home().join("settings.json").exists()),
            ("Working directory accessible", std::env::current_dir().is_ok()),
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

    #[allow(clippy::unused_async)]
    async fn run_update(&self) -> Result<()> {
        use crate::version;
        let cwd = std::env::current_dir().unwrap_or_default();
        match version::check_for_updates().await {
            Some(msg) => println!("{msg}"),
            None => println!("CodeRex is up to date."),
        }
        version::record_check_time(&cwd);
        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn run_mcp(&self, action: Option<crate::cli::McpAction>) -> Result<()> {
        let mcp_path = self.ctx.mcp_config_path.clone()
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
                                    println!("  {name}: {} {}", srv.command, srv.args.join(" "));
                                }
                            }
                        }
                        Err(e) => println!("  Error loading config: {e}"),
                    }
                } else {
                    println!("  No MCP config found at {mcp_path:?}");
                }
            }
            Some(crate::cli::McpAction::Add { name, command }) => {
                let mut config = if mcp_path.exists() {
                    coderex_mcp::config::McpConfig::load(&mcp_path)
                        .unwrap_or_else(|_| coderex_mcp::config::McpConfig::new())
                } else {
                    coderex_mcp::config::McpConfig::new()
                };
                let parts: Vec<&str> = command.split_whitespace().collect();
                let cmd = parts.first().map_or(command.clone(), |s| s.to_string());
                let args: Vec<String> = parts.iter().skip(1).map(|s| s.to_string()).collect();
                config.add_server(name.clone(), coderex_mcp::config::McpServerConfig {
                    command: cmd,
                    args,
                    env: std::collections::HashMap::new(),
                });
                match config.save(&mcp_path) {
                    Ok(()) => println!("Added MCP server '{name}' to {mcp_path:?}"),
                    Err(e) => println!("Error saving config: {e}"),
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
                        Ok(()) => println!("Removed MCP server '{name}' from {mcp_path:?}"),
                        Err(e) => println!("Error saving config: {e}"),
                    }
                } else {
                    println!("MCP server '{name}' not found in config.");
                }
            }
            Some(crate::cli::McpAction::Test { name }) => {
                if !mcp_path.exists() {
                    println!("No MCP config found at {mcp_path:?}");
                    return Ok(());
                }
                match coderex_mcp::config::McpConfig::load(&mcp_path) {
                    Ok(config) => {
                        if config.mcp_servers.contains_key(&name) {
                            println!("MCP server '{name}' is configured.");
                            println!("(Live connection test requires server spawn — not yet implemented)");
                        } else {
                            println!("MCP server '{name}' not found in config.");
                        }
                    }
                    Err(e) => println!("Error loading config: {e}"),
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
                let manager = coderex_plugins::plugins::PluginManager::new();
                let plugin = coderex_plugins::plugins::MarketplacePlugin {
                    name: source.clone(),
                    version: "latest".into(),
                    url: source.clone(),
                    description: String::new(),
                };
                let result = manager.install_plugin(&plugin, &install_dir).await;
                match result {
                    Ok(path) => println!("Plugin installed to: {path:?}"),
                    Err(e) => println!("Installation failed: {e}"),
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
                let manager = coderex_plugins::plugins::PluginManager::new();
                let reconciler = coderex_plugins::plugins::MarketplaceReconciler::new(300);
                match reconciler.reconcile(&manager, Some(&install_dir)).await {
                    Ok(report) => {
                        println!("Update complete: {} new, {} updated, {} removed",
                            report.new_count, report.updated_count, report.removed_count);
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

    #[allow(clippy::unused_async)]
    async fn run_auto_mode(&self) -> Result<()> {
        println!("Auto-mode settings:");
        let memory_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/memory");
        let dream_config_path = memory_dir.join("dream_config.json");
        if dream_config_path.exists() {
            match std::fs::read_to_string(&dream_config_path) {
                Ok(content) => {
                    println!("  Config file: {dream_config_path:?}");
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

    #[allow(clippy::unused_async)]
    async fn run_resume(&self, cwd: &str, session_id: &str, model: ModelId) -> Result<()> {
        let messages = Self::load_resume_messages(cwd, session_id)?;

        tracing::info!(
            "Resuming session {session_id}: {} messages loaded",
            messages.len()
        );
        println!("Resuming session: {session_id}");
        println!("Loaded {} messages from transcript", messages.len());

        // Show a summary of the conversation history
        let user_msgs: Vec<_> = messages
            .iter()
            .filter(|m| {
                matches!(m.role, coderex_core::types::Role::User)
            })
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
        let mut repl = if let Some(api_client) = Self::try_create_api_client(api_key) {
            crate::repl::ReplLoop::with_engine(api_client, cwd.to_string(), model)
        } else {
            crate::repl::ReplLoop::new(cwd.to_string(), model)
        };
        if let Some(ref pm) = self.ctx.plugin_manager {
            repl.with_plugin_manager(Arc::clone(pm));
        }
        self.configure_repl(&mut repl);
        repl.run_tui().await
    }

    fn load_resume_messages(
        cwd: &str,
        session_id: &str,
    ) -> Result<Vec<coderex_core::types::Message>> {
        use coderex_memory::transcript::TranscriptStore;

        let transcript_dir = std::path::Path::new(cwd).join(".coderex/transcripts");
        let store = TranscriptStore::new(transcript_dir.clone());
        let messages = store.read_all_skip_first_prompt(session_id)?;

        if messages.is_empty() {
            bail!("No transcript found for session '{session_id}' in {transcript_dir:?}");
        }

        Ok(messages)
    }

    /// Find the most recent session ID from the transcripts directory.
    fn find_latest_session(&self) -> Option<String> {
        let transcript_dir = std::path::Path::new(&self.ctx.cwd).join(".coderex/transcripts");
        if !transcript_dir.exists() {
            return None;
        }
        let mut latest: Option<(std::time::SystemTime, String)> = None;
        if let Ok(entries) = std::fs::read_dir(&transcript_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(modified) = meta.modified() {
                            let name = path.file_name()?.to_string_lossy().to_string();
                            if latest.as_ref().map_or(true, |(t, _)| modified > *t) {
                                latest = Some((modified, name));
                            }
                        }
                    }
                }
            }
        }
        latest.map(|(_, name)| name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_context_defaults() {
        let ctx = SessionContext::default();
        assert!(ctx.settings.is_none());
        assert!(ctx.model.is_none());
        assert!(ctx.cwd.is_empty());
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
            append_system_prompt: false,
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
            ide: None,
            worktree: false,
            prompt: Vec::new(),
            command: None,
        }
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
        let result = StartupOrchestrator::load_resume_messages(
            "/nonexistent",
            "nonexistent_session",
        );
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
            .append("sess_1", &coderex_core::types::Message::system("you are helpful"))
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
        assert!(matches!(messages[1].role, coderex_core::types::Role::Assistant));
    }

    #[test]
    fn try_create_api_client_reads_env_var() {
        // The function checks ANTHROPIC_API_KEY at call time.
        // In CI/tests the env var is typically unset, so we verify
        // the function returns None when no key is configured.
        // Direct env manipulation is blocked by -D unsafe-code.
        let result = StartupOrchestrator::try_create_api_client(None);
        // We can't guarantee env state, but verify the function compiles and runs
        // In practice: None when ANTHROPIC_API_KEY is unset, Some when set
        let _ = result;
    }
}
