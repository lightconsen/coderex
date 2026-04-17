//! Data directory management for CodeRex.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Manages all CodeRex data directories under a base path.
#[derive(Debug, Clone)]
pub struct DataDirectories {
    base: PathBuf,
}

impl DataDirectories {
    /// Create a new data directories manager.
    #[must_use]
    pub const fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// Ensure all required data directories exist.
    pub fn ensure_all(&self) -> std::io::Result<()> {
        self.ensure_dir("projects")?;
        self.ensure_dir("memory")?;
        self.ensure_dir("skills")?;
        self.ensure_dir("agents")?;
        self.ensure_dir("uploads")?;
        self.ensure_dir("telemetry")?;
        Ok(())
    }

    /// Ensure a single subdirectory exists under the base path.
    fn ensure_dir(&self, name: &str) -> std::io::Result<()> {
        let path = self.base.join(name);
        if !path.exists() {
            std::fs::create_dir_all(&path)?;
        }
        Ok(())
    }

    /// Path to managed settings file.
    #[must_use]
    pub fn managed_settings_path(&self) -> PathBuf {
        self.base.join("managed_settings.json")
    }

    /// Path to policy limits file.
    #[must_use]
    pub fn policy_limits_path(&self) -> PathBuf {
        self.base.join("policy_limits.json")
    }

    /// Path to sessions database.
    #[must_use]
    pub fn sessions_db_path(&self) -> PathBuf {
        self.base.join("sessions.db")
    }

    /// Path to a project-specific directory.
    #[must_use]
    pub fn project_dir(&self, project_hash: &str) -> PathBuf {
        self.base.join("projects").join(project_hash)
    }

    /// Path to the crash log file.
    #[must_use]
    pub fn crash_log_path(&self) -> PathBuf {
        self.base.join("crash.log")
    }

    /// Base path.
    #[must_use]
    pub fn base_path(&self) -> &Path {
        &self.base
    }
}

/// Managed settings with checksum verification.
#[derive(Debug, Serialize, Deserialize)]
pub struct ManagedSettings {
    /// The settings payload.
    pub settings: serde_json::Value,
    /// SHA-256 checksum of the settings JSON string.
    pub checksum: String,
    /// When these settings were fetched.
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

impl ManagedSettings {
    /// Verify the checksum matches the current settings.
    #[must_use]
    pub fn verify_checksum(&self) -> bool {
        let computed = Self::compute_checksum(&self.settings);
        computed == self.checksum
    }

    /// Compute the SHA-256 checksum of a settings value.
    #[must_use]
    pub fn compute_checksum(settings: &serde_json::Value) -> String {
        let mut hasher = Sha256::new();
        let json = serde_json::to_string(settings).unwrap_or_default();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Create new managed settings with computed checksum.
    #[must_use]
    pub fn new(settings: serde_json::Value) -> Self {
        Self {
            checksum: Self::compute_checksum(&settings),
            fetched_at: chrono::Utc::now(),
            settings,
        }
    }
}

/// API rate and budget policy limits.
#[derive(Debug, Serialize, Deserialize)]
pub struct PolicyLimits {
    /// Maximum requests per minute.
    pub max_requests_per_minute: Option<u32>,
    /// Maximum budget in USD per session.
    pub max_budget_usd_per_session: Option<f64>,
    /// Maximum tokens per turn.
    pub max_tokens_per_turn: Option<usize>,
    /// When these limits were fetched.
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_directories_ensure_all_creates_dirs() {
        let tmp = std::env::temp_dir().join("coderex_test_dirs");
        let _ = std::fs::remove_dir_all(&tmp);

        let dirs = DataDirectories::new(tmp.clone());
        dirs.ensure_all().unwrap();

        assert!(tmp.join("projects").exists());
        assert!(tmp.join("memory").exists());
        assert!(tmp.join("skills").exists());
        assert!(tmp.join("agents").exists());
        assert!(tmp.join("uploads").exists());
        assert!(tmp.join("telemetry").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn managed_settings_checksum_verifies() {
        let settings = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "permission_mode": "default"
        });
        let managed = ManagedSettings::new(settings.clone());
        assert!(managed.verify_checksum());
    }

    #[test]
    fn managed_settings_checksum_fails_on_tamper() {
        let settings = serde_json::json!({"model": "claude-sonnet-4-6"});
        let mut managed = ManagedSettings::new(settings);
        managed.settings = serde_json::json!({"model": "claude-opus-4-6"});
        assert!(!managed.verify_checksum());
    }

    #[test]
    fn project_dir_returns_correct_path() {
        let dirs = DataDirectories::new(PathBuf::from("/base"));
        let path = dirs.project_dir("abc123");
        assert_eq!(path, PathBuf::from("/base/projects/abc123"));
    }

    #[test]
    fn policy_limits_serializes_roundtrip() {
        let limits = PolicyLimits {
            max_requests_per_minute: Some(60),
            max_budget_usd_per_session: Some(10.0),
            max_tokens_per_turn: Some(100_000),
            fetched_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&limits).unwrap();
        let decoded: PolicyLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.max_requests_per_minute, Some(60));
        assert_eq!(decoded.max_budget_usd_per_session, Some(10.0));
    }
}
