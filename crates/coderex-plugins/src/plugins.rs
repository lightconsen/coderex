//! Plugin manager — loading, handles, marketplace reconciliation.

use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

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
}

/// A plugin manifest — parsed from plugin.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Marketplace connection state.
#[derive(Debug)]
pub struct MarketplaceConnection {
    /// The marketplace name.
    pub name: String,
    /// Base URL for the marketplace API.
    pub base_url: String,
    /// HTTP client for API calls.
    pub client: Client,
}

/// Manages all loaded plugins and marketplace state.
#[derive(Debug)]
pub struct PluginManager {
    /// Loaded plugins.
    plugins: DashMap<String, PluginHandle>,
    /// Marketplace connections.
    marketplaces: DashMap<String, MarketplaceConnection>,
    /// Whether a refresh is needed (new plugins installed).
    needs_refresh: AtomicBool,
    /// Plugin base directories.
    plugin_dirs: Vec<PathBuf>,
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
            marketplaces: DashMap::new(),
            needs_refresh: AtomicBool::new(false),
            plugin_dirs: Vec::new(),
        }
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
    pub fn load_plugin(&self, manifest: PluginManifest, path: PathBuf) {
        let handle = PluginHandle {
            name: manifest.name.clone(),
            version: manifest.version,
            state: PluginState::Active,
            path,
            commands: manifest.commands,
            skills: manifest.skills,
        };
        self.plugins.insert(handle.name.clone(), handle);
    }

    /// Unload a plugin.
    pub fn unload_plugin(&self, name: &str) -> bool {
        if let Some((_, handle)) = self.plugins.remove(name) {
            let _ = handle; // kept for audit
            self.needs_refresh.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
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
    pub fn register_marketplace(&self, name: String, base_url: String) {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        self.marketplaces.insert(name.clone(), MarketplaceConnection {
            name,
            base_url,
            client,
        });
    }

    /// Fetch available plugins from the marketplace.
    pub async fn fetch_marketplace_plugins(&self) -> Result<Vec<MarketplacePlugin>, String> {
        // Try each registered marketplace
        for entry in self.marketplaces.iter() {
            let conn = entry.value();
            let url = format!("{}/api/plugins", conn.base_url);

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
                    tracing::warn!(
                        "failed to connect to marketplace '{}': {e}",
                        conn.name
                    );
                }
            }
        }

        // Fall back to local reconciliation if no marketplace is available
        Ok(Vec::new())
    }

    /// Install a plugin from the marketplace.
    pub async fn install_plugin(
        &self,
        plugin: &MarketplacePlugin,
        install_dir: &PathBuf,
    ) -> Result<PathBuf, String> {
        let target_dir = install_dir.join(&plugin.name);

        // Mark as installing
        let handle = PluginHandle {
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            state: PluginState::Installing,
            path: target_dir.clone(),
            commands: Vec::new(),
            skills: Vec::new(),
        };
        self.plugins.insert(plugin.name.clone(), handle);

        // Download the plugin archive
        for entry in self.marketplaces.iter() {
            let conn = entry.value();
            let url = format!("{}/api/plugins/{}/download", conn.base_url, plugin.name);

            match conn.client.get(&url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        match response.bytes().await {
                            Ok(bytes) => {
                                // Create the plugin directory
                                std::fs::create_dir_all(&target_dir)
                                    .map_err(|e| format!("failed to create plugin dir: {e}"))?;

                                // Write the plugin manifest
                                let manifest = PluginManifest {
                                    name: plugin.name.clone(),
                                    version: plugin.version.clone(),
                                    description: plugin.description.clone(),
                                    author: String::new(),
                                    min_version: None,
                                    commands: Vec::new(),
                                    skills: Vec::new(),
                                };
                                let manifest_path = target_dir.join("plugin.json");
                                let manifest_json = serde_json::to_string_pretty(&manifest)
                                    .map_err(|e| format!("failed to serialize manifest: {e}"))?;
                                std::fs::write(&manifest_path, manifest_json)
                                    .map_err(|e| format!("failed to write manifest: {e}"))?;

                                // Write the downloaded archive
                                std::fs::write(target_dir.join("plugin.tar.gz"), &bytes)
                                    .map_err(|e| format!("failed to write plugin archive: {e}"))?;

                                // Update state to active
                                let handle = PluginHandle {
                                    name: plugin.name.clone(),
                                    version: plugin.version.clone(),
                                    state: PluginState::Active,
                                    path: target_dir.clone(),
                                    commands: Vec::new(),
                                    skills: Vec::new(),
                                };
                                self.plugins.insert(plugin.name.clone(), handle);

                                tracing::info!(
                                    "installed plugin '{}' v{}",
                                    plugin.name,
                                    plugin.version
                                );

                                // Signal that a refresh is needed
                                self.mark_needs_refresh();

                                return Ok(target_dir);
                            }
                            Err(e) => {
                                tracing::warn!("failed to read plugin archive: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to download plugin '{}': {e}", plugin.name);
                }
            }
        }

        Err(format!("failed to download plugin '{}' from any marketplace", plugin.name))
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
                local_plugins.iter().find(|lp| lp.name == mp.name).map(|lp| {
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
    pub async fn reconcile(&self, manager: &PluginManager, install_dir: Option<&PathBuf>) -> Result<ReconciliationReport, String> {
        let report = manager.reconcile().await?;

        if self.auto_install {
            if let Some(dir) = install_dir {
                let results = manager.auto_install_new(dir).await;
                let installed = results.iter().filter(|r| r.is_ok()).count();
                let failed = results.iter().filter(|r| r.is_err()).count();

                if installed > 0 {
                    tracing::info!(
                        "auto-installed {} plugins, {} failed",
                        installed,
                        failed
                    );
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
        };
        mgr.load_plugin(manifest, PathBuf::from("/plugins/test"));
        assert_eq!(mgr.len(), 1);
        assert_eq!(mgr.list_plugins(), vec!["test-plugin"]);

        let plugin = mgr.get_plugin("test-plugin").unwrap();
        assert_eq!(plugin.commands, vec!["cmd1"]);
        assert_eq!(plugin.skills, vec!["skill1"]);
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
            name: "a".into(), version: "1".into(),
            description: String::new(), author: String::new(),
            min_version: None, commands: vec!["cmd1".into()],
            skills: vec!["sk1".into()],
        };
        let m2 = PluginManifest {
            name: "b".into(), version: "1".into(),
            description: String::new(), author: String::new(),
            min_version: None, commands: vec!["cmd2".into()],
            skills: vec!["sk2".into()],
        };
        mgr.load_plugin(m1, PathBuf::new());
        mgr.load_plugin(m2, PathBuf::new());
        assert_eq!(mgr.all_commands().len(), 2);
        assert_eq!(mgr.all_skills().len(), 2);
    }

    #[test]
    fn marketplace_registration() {
        let mgr = PluginManager::new();
        mgr.register_marketplace("main".into(), "https://marketplace.example.com".into());
        // Just verifying it doesn't panic
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
}
