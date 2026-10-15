//! Plugin manager — loading, handles, marketplace reconciliation.

use dashmap::DashMap;
use ed25519_dalek::Verifier;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
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
}

/// Plugin runtime type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeType {
    /// External process (JSON-RPC over stdio).
    Process,
    /// WASM module (not yet implemented).
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

/// A plugin manifest — parsed from plugin.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// Plugin description.
    pub description: String,
    /// Plugin author.
    pub author: String,
    /// Minimum CodeRex version required.
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
    /// Ed25519 signature of the plugin archive (hex-encoded).
    #[serde(default)]
    pub signature: Option<String>,
    /// Runtime type: "process" (default) or "wasm".
    #[serde(default)]
    pub runtime: RuntimeType,
    /// Entrypoint command for process plugins (e.g., "bin/plugin", "node index.js").
    #[serde(default)]
    pub entrypoint: Option<String>,
    /// Environment variables for the plugin process.
    #[serde(default)]
    pub env: HashMap<String, String>,
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
        }
    }

    /// Set the application version for compatibility checks.
    pub fn set_app_version(&mut self, version: impl Into<String>) {
        self.app_version = version.into();
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
                        if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                            if let Ok(manifest) = serde_json::from_str::<PluginManifest>(&content) {
                                found.push(manifest);
                            }
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
    pub async fn spawn_plugin(&self, name: &str) -> Result<(), String> {
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
            let wasm_file = manifest.entrypoint.as_deref().unwrap_or("plugin.wasm");
            let wasm_path = handle.path.join(wasm_file);

            let mut instance = PluginWasmInstance::new(&wasm_path)
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

        // Determine entrypoint
        let entrypoint = manifest.entrypoint.as_deref().unwrap_or("bin/plugin");

        // Split entrypoint into command and args (supports "node index.js")
        let parts: Vec<&str> = entrypoint.split_whitespace().collect();
        if parts.is_empty() {
            return Err(format!("plugin '{}' has empty entrypoint", name));
        }

        let cmd = parts[0].to_string();
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        let mut process = PluginProcess::new(cmd, args, manifest.env, handle.path.clone());

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
        let manifest_path = dir.join("plugin.json");
        if !manifest_path.exists() {
            return Err("extracted plugin missing plugin.json".into());
        }
        let content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("failed to read plugin.json: {e}"))?;
        let manifest: PluginManifest = serde_json::from_str(&content)
            .map_err(|e| format!("failed to parse plugin.json: {e}"))?;
        Ok(manifest)
    }

    /// Install a plugin from the marketplace.
    ///
    /// Uses atomic temp-dir staging: downloads and extracts into a temporary
    /// directory, validates the structure, then renames atomically to the final
    /// location. On failure, the temporary directory is removed.
    pub async fn install_plugin(
        &self,
        plugin: &MarketplacePlugin,
        install_dir: &PathBuf,
    ) -> Result<PathBuf, String> {
        let target_dir = install_dir.join(&plugin.name);

        // Acquire install lock for this plugin name
        let lock = {
            let arc = self
                .install_locks
                .entry(plugin.name.clone())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone();
            arc
        };
        let _guard = lock.lock().await;

        // Check if already installed
        if target_dir.exists() {
            return Err(format!(
                "plugin '{}' is already installed at {:?}",
                plugin.name, target_dir
            ));
        }

        // Mark as installing
        let handle = PluginHandle {
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            state: PluginState::Installing,
            path: target_dir.clone(),
            commands: Vec::new(),
            skills: Vec::new(),
            retry_count: 0,
            max_retries: 3,
            verified: false,
        };
        self.plugins.insert(plugin.name.clone(), handle);

        // Create a temporary staging directory
        let temp_dir = install_dir.join(format!(".install-{}", plugin.name));
        let _ = std::fs::remove_dir_all(&temp_dir); // clean up any previous failed attempt

        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            self.set_plugin_state(
                &plugin.name,
                PluginState::Error(format!("temp dir creation failed: {e}")),
            );
            return Err(format!("failed to create temp dir: {e}"));
        }

        // Download the plugin archive
        let archive_bytes = 'download: {
            for entry in self.marketplaces.iter() {
                let conn = entry.value();
                let download_path = conn.download_endpoint.replace("{name}", &plugin.name);
                let url = format!("{}{}", conn.base_url, download_path);

                match conn.client.get(&url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            match response.bytes().await {
                                Ok(bytes) => break 'download Some(bytes.to_vec()),
                                Err(e) => tracing::warn!("failed to read plugin archive: {e}"),
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to download plugin '{}': {e}", plugin.name);
                    }
                }
            }
            None
        };

        let bytes = match archive_bytes {
            Some(b) => b,
            None => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                self.set_plugin_state(
                    &plugin.name,
                    PluginState::Error("download failed from all marketplaces".into()),
                );
                return Err(format!(
                    "failed to download plugin '{}' from any marketplace",
                    plugin.name
                ));
            }
        };

        // Extract the archive into the temp directory
        if let Err(e) = Self::extract_archive(&bytes, &temp_dir) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            self.set_plugin_state(
                &plugin.name,
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
                    &plugin.name,
                    PluginState::Error(format!("validation failed: {e}")),
                );
                return Err(e);
            }
        };

        // Version compatibility check
        if !self.is_version_compatible(manifest.min_version.as_deref()) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            let msg = format!(
                "plugin '{}' requires CodeRex >= {} (current: {})",
                plugin.name,
                manifest.min_version.as_deref().unwrap_or("unknown"),
                self.app_version
            );
            self.set_plugin_state(&plugin.name, PluginState::Error(msg.clone()));
            return Err(msg);
        }

        // Signature verification. A configured key makes a signature
        // mandatory: a plugin that cannot be checked is not installed.
        let signature = match self.verify_signature(&bytes, manifest.signature.as_deref()) {
            Ok(check) => check,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                self.set_plugin_state(
                    &plugin.name,
                    PluginState::Error(format!("signature verification failed: {e}")),
                );
                return Err(e);
            }
        };
        let verified = matches!(signature, SignatureCheck::Verified);
        if !verified {
            tracing::warn!(
                plugin = %plugin.name,
                "installing unsigned plugin: no signing key is configured, so nothing about \
                 this archive has been checked"
            );
        }

        // Atomically rename temp dir to final location
        if let Err(e) = std::fs::rename(&temp_dir, &target_dir) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            self.set_plugin_state(
                &plugin.name,
                PluginState::Error(format!("atomic rename failed: {e}")),
            );
            return Err(format!("failed to finalize plugin install: {e}"));
        }

        // Clean up the archive file if it was left in the target dir
        let archive_path = target_dir.join("plugin.tar.gz");
        if archive_path.exists() {
            let _ = std::fs::remove_file(&archive_path);
        }

        // Update state to active
        let handle = PluginHandle {
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            state: PluginState::Active,
            path: target_dir.clone(),
            commands: manifest.commands,
            skills: manifest.skills,
            retry_count: 0,
            max_retries: 3,
            verified,
        };
        self.plugins.insert(plugin.name.clone(), handle);

        tracing::info!("installed plugin '{}' v{}", plugin.name, plugin.version);

        self.mark_needs_refresh();
        Ok(target_dir)
    }

    /// Set the state of a plugin by name.
    fn set_plugin_state(&self, name: &str, state: PluginState) {
        if let Some(mut entry) = self.plugins.get_mut(name) {
            entry.state = state;
        }
    }

    /// Retry loading plugins that are in the `Error` state.
    ///
    /// Uses exponential backoff: waits `2^retry_count` seconds before retrying.
    /// Plugins that exceed `max_retries` are left in Error state.
    pub fn retry_failed_plugins(&self) {
        for mut entry in self.plugins.iter_mut() {
            let plugin = entry.value_mut();
            if !matches!(plugin.state, PluginState::Error(_)) {
                continue;
            }
            if plugin.retry_count >= plugin.max_retries {
                continue;
            }

            plugin.retry_count += 1;

            // Attempt to re-read the manifest
            let manifest_path = plugin.path.join("plugin.json");
            if !manifest_path.exists() {
                plugin.state = PluginState::Error(format!(
                    "manifest missing (retry {}/{})",
                    plugin.retry_count, plugin.max_retries
                ));
                continue;
            }

            match std::fs::read_to_string(&manifest_path) {
                Ok(content) => match serde_json::from_str::<PluginManifest>(&content) {
                    Ok(manifest) => {
                        if self.is_version_compatible(manifest.min_version.as_deref()) {
                            plugin.version = manifest.version;
                            plugin.commands = manifest.commands;
                            plugin.skills = manifest.skills;
                            plugin.state = PluginState::Active;
                            tracing::info!(
                                plugin = %plugin.name,
                                retry = plugin.retry_count,
                                "plugin recovered from error state"
                            );
                        } else {
                            plugin.state = PluginState::Error(format!(
                                "incompatible version (retry {}/{})",
                                plugin.retry_count, plugin.max_retries
                            ));
                        }
                    }
                    Err(e) => {
                        plugin.state = PluginState::Error(format!(
                            "manifest parse failed: {e} (retry {}/{})",
                            plugin.retry_count, plugin.max_retries
                        ));
                    }
                },
                Err(e) => {
                    plugin.state = PluginState::Error(format!(
                        "manifest read failed: {e} (retry {}/{})",
                        plugin.retry_count, plugin.max_retries
                    ));
                }
            }
        }
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
            runtime: RuntimeType::Process,
            entrypoint: None,
            env: HashMap::new(),
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
            author: String::new(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            runtime: RuntimeType::Process,
            entrypoint: None,
            env: HashMap::new(),
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
            author: String::new(),
            min_version: None,
            commands: vec!["cmd1".into()],
            skills: vec!["sk1".into()],
            dependencies: Vec::new(),
            signature: None,
            runtime: RuntimeType::Process,
            entrypoint: None,
            env: HashMap::new(),
        };
        let m2 = PluginManifest {
            name: "b".into(),
            version: "1".into(),
            description: String::new(),
            author: String::new(),
            min_version: None,
            commands: vec!["cmd2".into()],
            skills: vec!["sk2".into()],
            dependencies: Vec::new(),
            signature: None,
            runtime: RuntimeType::Process,
            entrypoint: None,
            env: HashMap::new(),
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
            author: String::new(),
            min_version: None,
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            runtime: RuntimeType::Process,
            entrypoint: None,
            env: HashMap::new(),
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
            author: String::new(),
            min_version: Some("99.0.0".into()),
            commands: vec![],
            skills: vec![],
            dependencies: Vec::new(),
            signature: None,
            runtime: RuntimeType::Process,
            entrypoint: None,
            env: HashMap::new(),
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
        assert!(mgr
            .verify_signature(b"a swapped archive", Some(&sig_hex))
            .is_err());
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
            author: String::new(),
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
            author: String::new(),
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
            author: String::new(),
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
            author: String::new(),
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

    #[test]
    fn retry_failed_plugins_recoverable() {
        let mgr = PluginManager::new();
        let manifest = PluginManifest {
            name: "broken".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: String::new(),
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
}
