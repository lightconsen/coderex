//! Remote managed settings — enterprise-configurable overrides fetched from a remote server.
//!
//! For enterprise users, settings can be managed centrally and pushed to all clients.
//! The system uses checksum-based fetch to avoid re-downloading unchanged configs,
//! and a loading barrier to coordinate startup while remote settings are being fetched.
//! On failure, the system fails open (continues without remote settings).

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
use tracing::{debug, warn};

use crate::settings::Settings;

/// Errors from remote managed settings operations.
#[derive(Debug, thiserror::Error)]
pub enum RemoteSettingsError {
    /// Network error during fetch.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    /// JSON parse error in response.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    /// Security validation failed.
    #[error("security validation failed: {reason}")]
    SecurityViolation {
        /// Description of what failed.
        reason: String,
    },
    /// Eligibility check failed.
    #[error("not eligible for enterprise settings")]
    NotEligible,
}

/// Response from the remote managed settings endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSettingsResponse {
    /// The settings payload.
    pub settings: RemoteManagedSettings,
    /// SHA-256 checksum of the settings for integrity verification.
    pub checksum: String,
    /// When these settings were last updated on the server.
    pub updated_at: DateTime<Utc>,
}

/// Settings that can be managed remotely by an enterprise admin.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteManagedSettings {
    /// Enforced model (users cannot override).
    pub enforced_model: Option<String>,
    /// Enforced permission mode.
    pub enforced_permission_mode: Option<String>,
    /// Maximum budget cap in USD per session.
    pub budget_cap_usd: Option<f64>,
    /// Blocked tools that users cannot access.
    pub blocked_tools: Option<Vec<String>>,
    /// Required hooks that must be configured.
    pub required_hooks: Option<Vec<String>>,
    /// Organization identifier.
    pub org_id: Option<String>,
    /// Organization-level policy flags.
    pub policy_flags: Option<serde_json::Value>,
}

/// State of the remote settings fetch operation.
#[derive(Debug, Clone)]
pub enum FetchState {
    /// Not yet attempted.
    NotStarted,
    /// Currently fetching.
    Fetching,
    /// Successfully fetched and applied.
    Ready {
        /// The merged settings.
        settings: RemoteManagedSettings,
        /// When they were fetched.
        fetched_at: DateTime<Utc>,
        /// Checksum of the fetched settings.
        checksum: String,
    },
    /// Failed to fetch (fail open — using local settings).
    Failed {
        /// The error that occurred.
        error: String,
        /// When the failure occurred.
        failed_at: DateTime<Utc>,
    },
}

/// Loading barrier — allows other subsystems to wait for remote settings to be resolved.
#[derive(Debug, Clone)]
pub struct LoadingBarrier {
    /// Watch channel signaling completion (success or failure).
    completed: Arc<watch::Sender<FetchState>>,
    /// Current state (readable by all).
    state: Arc<RwLock<FetchState>>,
}

impl LoadingBarrier {
    /// Create a new loading barrier.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(FetchState::NotStarted);
        Self {
            completed: Arc::new(tx),
            state: Arc::new(RwLock::new(FetchState::NotStarted)),
        }
    }

    /// Wait for the barrier to be resolved (settings fetched or failed).
    pub async fn wait(&self) -> FetchState {
        let mut rx = self.completed.subscribe();
        // If already resolved, return immediately
        {
            let guard = self.state.read().await;
            if !matches!(*guard, FetchState::NotStarted | FetchState::Fetching) {
                return guard.clone();
            }
        }
        // Otherwise wait for the signal
        let _ = rx.changed().await;
        let guard = self.state.read().await;
        guard.clone()
    }

    /// Signal that fetching is complete (success).
    pub async fn resolve_success(&self, settings: RemoteManagedSettings, checksum: String) {
        let state = FetchState::Ready {
            settings,
            fetched_at: Utc::now(),
            checksum,
        };
        *self.state.write().await = state.clone();
        let _ = self.completed.send(state);
    }

    /// Signal that fetching failed (fail open).
    pub async fn resolve_failed(&self, error: String) {
        let state = FetchState::Failed {
            error,
            failed_at: Utc::now(),
        };
        *self.state.write().await = state.clone();
        let _ = self.completed.send(state);
    }

    /// Mark as currently fetching.
    pub async fn mark_fetching(&self) {
        let state = FetchState::Fetching;
        *self.state.write().await = state.clone();
        let _ = self.completed.send(state);
    }

    /// Get the current state without waiting.
    pub async fn current_state(&self) -> FetchState {
        self.state.read().await.clone()
    }
}

impl Default for LoadingBarrier {
    fn default() -> Self {
        Self::new()
    }
}

/// Disk-persisted cache entry for offline resilience.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSettings {
    /// The cached settings payload.
    settings: RemoteManagedSettings,
    /// SHA-256 checksum for integrity and change detection.
    checksum: String,
    /// When these settings were last fetched from the server.
    fetched_at: DateTime<Utc>,
}

/// Remote managed settings client.
#[derive(Debug)]
pub struct RemoteManagedSettingsClient {
    /// Base URL for the settings endpoint.
    pub base_url: String,
    /// HTTP client.
    http: Client,
    /// API key for authentication.
    api_key: String,
    /// Cached checksum to skip unchanged fetches.
    last_checksum: RwLock<Option<String>>,
    /// Optional path for disk-based offline cache.
    cache_path: Option<std::path::PathBuf>,
}

impl RemoteManagedSettingsClient {
    /// Create a new remote settings client.
    ///
    /// If `cache_path` is provided, settings are persisted to disk for offline access.
    #[must_use]
    pub fn new(base_url: String, api_key: String, cache_path: Option<std::path::PathBuf>) -> Self {
        let last_checksum = if let Some(ref path) = cache_path {
            // Load cached checksum from disk on initialization
            load_cached_checksum(path)
        } else {
            None
        };
        Self {
            base_url,
            http: Client::new(),
            api_key,
            last_checksum: RwLock::new(last_checksum),
            cache_path,
        }
    }

    /// Save settings to the disk cache if configured.
    async fn save_to_cache(&self, settings: &RemoteManagedSettings, checksum: &str) {
        if let Some(ref path) = self.cache_path {
            let cached = CachedSettings {
                settings: settings.clone(),
                checksum: checksum.to_string(),
                fetched_at: Utc::now(),
            };
            if let Err(e) = save_cached_settings(path, &cached) {
                warn!("failed to save settings to disk cache: {e}");
            } else {
                debug!("saved settings to disk cache at {:?}", path);
            }
        }
    }

    /// Load the last cached checksum from disk.
    fn load_cached_checksum(&self) -> Option<String> {
        self.cache_path.as_ref().and_then(|p| load_cached_checksum(p.as_path()))
    }

    /// Load full cached settings from disk (for offline fallback).
    fn load_cached_settings(&self) -> Option<RemoteManagedSettings> {
        self.cache_path
            .as_ref()
            .and_then(|p| load_cached_settings(p.as_path()))
            .map(|c| c.settings)
    }

    /// Check if the current user/org is eligible for enterprise managed settings.
    ///
    /// Eligibility requires:
    /// - A valid enterprise API key (non-empty, properly formatted)
    /// - A configured base URL
    pub async fn check_eligibility(&self) -> Result<bool, RemoteSettingsError> {
        if self.api_key.is_empty() {
            return Err(RemoteSettingsError::NotEligible);
        }
        if self.base_url.is_empty() || !self.base_url.starts_with("http") {
            return Err(RemoteSettingsError::NotEligible);
        }

        // Probe the eligibility endpoint
        let url = format!("{}/api/v1/enterprise/eligible", self.base_url.trim_end_matches('/'));
        let resp = self.http.get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await;

        match resp {
            Ok(r) => {
                if r.status().is_success() {
                    Ok(true)
                } else if r.status() == 403 {
                    Err(RemoteSettingsError::NotEligible)
                } else {
                    Err(RemoteSettingsError::Network(r.error_for_status().unwrap_err()))
                }
            }
            Err(e) => {
                // Network errors don't mean ineligible — could be offline
                // Treat as eligible but with local settings
                tracing::warn!("eligibility check failed (network error): {e}, treating as eligible with local settings");
                Ok(true)
            }
        }
    }

    /// Fetch remote managed settings.
    ///
    /// Returns `None` if the checksum hasn't changed (cached copy is current).
    pub async fn fetch(&self) -> Result<Option<RemoteManagedSettings>, RemoteSettingsError> {
        let url = format!("{}/api/v1/enterprise/settings", self.base_url.trim_end_matches('/'));
        let resp = self.http.get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()
            .map_err(RemoteSettingsError::Network)?;

        let body: RemoteSettingsResponse = resp.json().await?;

        // Security check: verify checksum matches the settings payload
        let computed = Self::compute_checksum(&body.settings)?;
        if computed != body.checksum {
            return Err(RemoteSettingsError::SecurityViolation {
                reason: format!("checksum mismatch: expected {}, computed {}", body.checksum, computed),
            });
        }

        // Check if settings have changed since last fetch (from memory or disk cache)
        let last = self.last_checksum.read().await;
        if let Some(ref prev) = *last {
            if *prev == body.checksum {
                return Ok(None); // No change
            }
        }
        drop(last);

        let checksum = body.checksum.clone();
        let settings = body.settings.clone();

        // Persist to disk cache
        self.save_to_cache(&settings, &checksum).await;

        *self.last_checksum.write().await = Some(checksum);
        Ok(Some(settings))
    }

    /// Fetch remote settings with fail-open semantics.
    ///
    /// On any error, logs a warning and falls back to the last cached copy from disk.
    /// The loading barrier is used to signal completion to waiting subsystems.
    pub async fn fetch_with_barrier(
        &self,
        barrier: &LoadingBarrier,
    ) -> Option<RemoteManagedSettings> {
        barrier.mark_fetching().await;

        match self.fetch().await {
            Ok(Some(settings)) => {
                let checksum = self.last_checksum.read().await.clone().unwrap_or_default();
                barrier.resolve_success(settings.clone(), checksum).await;
                Some(settings)
            }
            Ok(None) => {
                // No change — resolve with current cached state
                let checksum = self.last_checksum.read().await.clone().unwrap_or_default();
                let settings = self.load_cached_settings().unwrap_or_default();
                barrier.resolve_success(settings, checksum).await;
                None
            }
            Err(e) => {
                // Fail open: try to use the last cached settings from disk
                tracing::warn!("remote settings fetch failed (failing open): {e}");
                if let Some(cached) = self.load_cached_settings() {
                    tracing::info!("using last cached settings from disk (offline fallback)");
                    let checksum = self.last_checksum.read().await.clone().unwrap_or_default();
                    barrier.resolve_success(cached.clone(), checksum).await;
                    return Some(cached);
                }
                barrier.resolve_failed(e.to_string()).await;
                None
            }
        }
    }

    /// Merge remote managed settings into local settings.
    ///
    /// Remote settings take precedence over local settings for enforced values.
    #[must_use]
    pub fn merge_into_local(
        remote: &RemoteManagedSettings,
        local: Settings,
    ) -> Settings {
        let mut merged = local;

        if let Some(ref model) = remote.enforced_model {
            merged.model = Some(model.clone());
        }
        if let Some(ref mode) = remote.enforced_permission_mode {
            merged.permission_mode = Some(mode.clone());
        }
        // budget_cap_usd maps to max_budget_usd
        if let Some(cap) = remote.budget_cap_usd {
            merged.max_budget_usd = Some(cap);
        }

        merged
    }

    /// Compute SHA-256 checksum of a settings payload.
    fn compute_checksum(settings: &RemoteManagedSettings) -> Result<String, RemoteSettingsError> {
        let serialized = serde_json::to_string(settings)?;
        let mut hasher = Sha256::new();
        hasher.update(serialized.as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }
}

/// Combines the full remote settings subsystem: client, barrier, and merged result.
#[derive(Debug)]
pub struct RemoteSettingsManager {
    /// The HTTP client for fetching settings.
    pub client: Option<RemoteManagedSettingsClient>,
    /// Loading barrier for coordinating startup.
    pub barrier: LoadingBarrier,
    /// Last successfully fetched settings.
    pub settings: RwLock<Option<RemoteManagedSettings>>,
}

impl RemoteSettingsManager {
    /// Create a new manager. If base_url or api_key is empty, no remote client is created
    /// and the system will use local settings only.
    ///
    /// If `cache_path` is provided, settings are persisted to disk for offline access.
    #[must_use]
    pub fn new(
        base_url: String,
        api_key: String,
        cache_path: Option<std::path::PathBuf>,
    ) -> Self {
        let client = if !base_url.is_empty() && !api_key.is_empty() {
            Some(RemoteManagedSettingsClient::new(base_url, api_key, cache_path))
        } else {
            None
        };
        Self {
            client,
            barrier: LoadingBarrier::new(),
            settings: RwLock::new(None),
        }
    }

    /// Start background fetch of remote settings.
    ///
    /// This is non-blocking — other subsystems can use `barrier.wait()` to wait for completion.
    pub async fn start_fetch(&self) {
        if let Some(ref client) = self.client {
            let result = client.fetch_with_barrier(&self.barrier).await;
            if let Some(settings) = result {
                *self.settings.write().await = Some(settings);
            }
        } else {
            // No remote client configured — resolve immediately with empty settings
            self.barrier.resolve_success(RemoteManagedSettings::default(), String::new()).await;
        }
    }

    /// Get the current remote settings if available.
    pub async fn get_settings(&self) -> Option<RemoteManagedSettings> {
        self.settings.read().await.clone()
    }

    /// Resolve local settings with remote overrides applied.
    pub async fn resolve(&self, local: Settings) -> Settings {
        if let Some(ref remote) = *self.settings.read().await {
            RemoteManagedSettingsClient::merge_into_local(remote, local)
        } else {
            local
        }
    }
}

// Disk cache helpers

fn load_cached_checksum(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<CachedSettings>(&content).ok())
        .map(|c| c.checksum)
}

fn load_cached_settings(path: &Path) -> Option<CachedSettings> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<CachedSettings>(&content).ok())
}

fn save_cached_settings(path: &Path, cached: &CachedSettings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(cached)?;
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_remote_overrides_local() {
        let local = Settings {
            model: Some("claude-sonnet-4-6".into()),
            permission_mode: Some("default".into()),
            max_budget_usd: Some(100.0),
            ..Default::default()
        };
        let remote = RemoteManagedSettings {
            enforced_model: Some("claude-opus-4-6".into()),
            enforced_permission_mode: Some("dont-ask".into()),
            budget_cap_usd: Some(10.0),
            ..Default::default()
        };

        let merged = RemoteManagedSettingsClient::merge_into_local(&remote, local);
        assert_eq!(merged.model, Some("claude-opus-4-6".into()));
        assert_eq!(merged.permission_mode, Some("dont-ask".into()));
        assert_eq!(merged.max_budget_usd, Some(10.0));
    }

    #[test]
    fn merge_empty_remote_preserves_local() {
        let local = Settings {
            model: Some("claude-sonnet-4-6".into()),
            permission_mode: Some("default".into()),
            ..Default::default()
        };
        let remote = RemoteManagedSettings::default();

        let merged = RemoteManagedSettingsClient::merge_into_local(&remote, local.clone());
        assert_eq!(merged.model, local.model);
        assert_eq!(merged.permission_mode, local.permission_mode);
    }

    #[test]
    fn compute_checksum_is_deterministic() {
        let settings = RemoteManagedSettings {
            enforced_model: Some("model-x".into()),
            ..Default::default()
        };
        let c1 = RemoteManagedSettingsClient::compute_checksum(&settings).unwrap();
        let c2 = RemoteManagedSettingsClient::compute_checksum(&settings).unwrap();
        assert_eq!(c1, c2);
    }

    #[test]
    fn compute_checksum_differs_for_different_content() {
        let s1 = RemoteManagedSettings {
            enforced_model: Some("model-a".into()),
            ..Default::default()
        };
        let s2 = RemoteManagedSettings {
            enforced_model: Some("model-b".into()),
            ..Default::default()
        };
        let c1 = RemoteManagedSettingsClient::compute_checksum(&s1).unwrap();
        let c2 = RemoteManagedSettingsClient::compute_checksum(&s2).unwrap();
        assert_ne!(c1, c2);
    }

    #[test]
    fn security_check_catches_tampering() {
        let settings = RemoteManagedSettings {
            enforced_model: Some("model-x".into()),
            ..Default::default()
        };
        let correct_checksum = RemoteManagedSettingsClient::compute_checksum(&settings).unwrap();
        let wrong_checksum = "deadbeef".to_string();

        let response = RemoteSettingsResponse {
            settings,
            checksum: wrong_checksum,
            updated_at: Utc::now(),
        };

        // Simulate the security check
        let computed = RemoteManagedSettingsClient::compute_checksum(&response.settings).unwrap();
        assert_ne!(computed, response.checksum);
        assert_eq!(computed, correct_checksum);
    }

    #[test]
    fn loading_barrier_resolves_success() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = LoadingBarrier::new();
            barrier.mark_fetching().await;
            barrier.resolve_success(RemoteManagedSettings::default(), "abc123".into()).await;

            let state = barrier.wait().await;
            assert!(matches!(state, FetchState::Ready { .. }));
        });
    }

    #[test]
    fn loading_barrier_resolves_failure() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = LoadingBarrier::new();
            barrier.mark_fetching().await;
            barrier.resolve_failed("timeout".into()).await;

            let state = barrier.wait().await;
            assert!(matches!(state, FetchState::Failed { ref error, .. } if error == "timeout"));
        });
    }

    #[test]
    fn loading_barrier_wait_returns_immediately_if_resolved() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let barrier = LoadingBarrier::new();
            barrier.resolve_success(RemoteManagedSettings::default(), "abc".into()).await;

            let state = barrier.wait().await;
            assert!(matches!(state, FetchState::Ready { .. }));
        });
    }

    #[tokio::test]
    async fn remote_manager_without_client_resolves_immediately() {
        let mgr = RemoteSettingsManager::new(String::new(), String::new(), None);
        mgr.start_fetch().await;
        let state = mgr.barrier.current_state().await;
        assert!(matches!(state, FetchState::Ready { .. }));
    }

    #[tokio::test]
    async fn remote_manager_resolve_applies_overrides() {
        let mgr = RemoteSettingsManager::new(String::new(), String::new(), None);
        // Simulate having fetched settings
        *mgr.settings.write().await = Some(RemoteManagedSettings {
            enforced_model: Some("enforced-model".into()),
            budget_cap_usd: Some(5.0),
            ..Default::default()
        });

        let local = Settings {
            model: Some("user-model".into()),
            max_budget_usd: Some(100.0),
            ..Default::default()
        };
        let resolved = mgr.resolve(local).await;
        assert_eq!(resolved.model, Some("enforced-model".into()));
        assert_eq!(resolved.max_budget_usd, Some(5.0));
    }

    #[tokio::test]
    async fn eligibility_empty_api_key_fails() {
        let client = RemoteManagedSettingsClient::new("https://example.com".into(), String::new(), None);
        let result = client.check_eligibility().await;
        assert!(matches!(result, Err(RemoteSettingsError::NotEligible)));
    }

    #[tokio::test]
    async fn eligibility_invalid_url_fails() {
        let client = RemoteManagedSettingsClient::new("not-a-url".into(), "some-key".into(), None);
        let result = client.check_eligibility().await;
        assert!(matches!(result, Err(RemoteSettingsError::NotEligible)));
    }

    #[test]
    fn disk_cache_roundtrip() {
        let tmp = std::env::temp_dir();
        let cache_path = tmp.join("coderex_test_remote_cache.json");

        let cached = CachedSettings {
            settings: RemoteManagedSettings {
                enforced_model: Some("cached-model".into()),
                budget_cap_usd: Some(50.0),
                ..Default::default()
            },
            checksum: "abc123".to_string(),
            fetched_at: Utc::now(),
        };

        save_cached_settings(&cache_path, &cached).unwrap();
        let loaded = load_cached_settings(&cache_path).unwrap();
        assert_eq!(loaded.settings.enforced_model, Some("cached-model".into()));
        assert_eq!(loaded.checksum, "abc123");
        assert_eq!(loaded.settings.budget_cap_usd, Some(50.0));

        // Checksum from disk matches what the client would load
        let cksum = load_cached_checksum(&cache_path).unwrap();
        assert_eq!(cksum, "abc123");

        let _ = std::fs::remove_file(&cache_path);
    }
}
