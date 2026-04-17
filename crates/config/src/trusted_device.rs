//! Trusted device mechanism — device registration, trust verification, and enhanced authentication.
//!
//! Devices must be registered and verified before they can access enterprise features.
//! Trust is established via a device identity (machine-bound key) and an admin approval flow.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

/// Errors from trusted device operations.
#[derive(Debug, thiserror::Error)]
pub enum TrustedDeviceError {
    /// Device is not trusted.
    #[error("device {device_id} is not trusted")]
    DeviceNotTrusted {
        /// The untrusted device ID.
        device_id: String,
    },
    /// Device already registered.
    #[error("device {device_id} already registered")]
    DeviceAlreadyRegistered {
        /// The device ID.
        device_id: String,
    },
    /// I/O error during store operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Verification token invalid.
    #[error("verification token invalid")]
    InvalidToken,
    /// Verification token expired.
    #[error("verification token expired at {expired_at}")]
    TokenExpired {
        /// When the token expired.
        expired_at: DateTime<Utc>,
    },
}

/// Trust status of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustStatus {
    /// Device is registered but not yet verified.
    Pending,
    /// Device has been verified and is trusted.
    Trusted,
    /// Device trust has been revoked.
    Revoked,
}

/// A registered device identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// Unique device identifier (SHA-256 of machine fingerprint).
    pub device_id: String,
    /// Human-readable device name.
    pub device_name: String,
    /// Current trust status.
    pub trust_status: TrustStatus,
    /// Verification token (present when pending, used for admin approval).
    pub verification_token: Option<String>,
    /// When this device was first registered.
    pub registered_at: DateTime<Utc>,
    /// When this device was last verified or status changed.
    pub last_verified_at: Option<DateTime<Utc>>,
    /// Organization this device belongs to (for enterprise scoping).
    pub org_id: Option<String>,
}

impl DeviceRecord {
    /// Check if this device is currently trusted.
    #[must_use]
    pub fn is_trusted(&self) -> bool {
        self.trust_status == TrustStatus::Trusted
    }
}

/// Trusted device registry — manages device registration and verification.
#[derive(Debug)]
pub struct TrustedDeviceRegistry {
    /// Path to the device store file.
    store_path: PathBuf,
    /// In-memory cache of devices.
    devices: HashMap<String, DeviceRecord>,
    /// Token expiry duration.
    token_expiry_hours: i64,
}

impl TrustedDeviceRegistry {
    /// Create a new registry with the given store path.
    #[must_use]
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            devices: HashMap::new(),
            token_expiry_hours: 48,
        }
    }

    /// Create with a custom token expiry duration.
    #[must_use]
    pub fn with_token_expiry(mut self, hours: i64) -> Self {
        self.token_expiry_hours = hours;
        self
    }

    /// Load the device store from disk.
    pub fn load(&mut self) -> Result<(), TrustedDeviceError> {
        if !self.store_path.exists() {
            return Ok(());
        }
        let content = std::fs::read_to_string(&self.store_path)?;
        let devices: HashMap<String, DeviceRecord> = serde_json::from_str(&content)?;
        self.devices = devices;
        Ok(())
    }

    /// Persist the device store to disk.
    pub fn save(&self) -> Result<(), TrustedDeviceError> {
        if let Some(parent) = self.store_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&self.devices)?;
        std::fs::write(&self.store_path, content)?;
        Ok(())
    }

    /// Register a new device. Returns a verification token.
    ///
    /// The device starts in `Pending` status and must be verified before use.
    pub fn register_device(
        &mut self,
        device_name: &str,
        org_id: Option<&str>,
    ) -> Result<String, TrustedDeviceError> {
        let device_id = Self::generate_device_id();

        if self.devices.contains_key(&device_id) {
            return Err(TrustedDeviceError::DeviceAlreadyRegistered { device_id });
        }

        let token = Self::generate_verification_token(&device_id);
        let now = Utc::now();

        let record = DeviceRecord {
            device_id: device_id.clone(),
            device_name: device_name.to_string(),
            trust_status: TrustStatus::Pending,
            verification_token: Some(token.clone()),
            registered_at: now,
            last_verified_at: None,
            org_id: org_id.map(String::from),
        };

        self.devices.insert(device_id, record);
        self.save()?;
        Ok(token)
    }

    /// Verify a device using its verification token.
    ///
    /// Marks the device as `Trusted` if the token is valid and not expired.
    pub fn verify_device(&mut self, token: &str) -> Result<(), TrustedDeviceError> {
        let now = Utc::now();

        // Find the device with this token
        let device = self
            .devices
            .values_mut()
            .find(|d| d.verification_token.as_deref() == Some(token));

        let device = match device {
            Some(d) => d,
            None => return Err(TrustedDeviceError::InvalidToken),
        };

        if device.trust_status != TrustStatus::Pending {
            return Err(TrustedDeviceError::InvalidToken);
        }

        // Check token expiry
        let expires_at = device.registered_at + chrono::Duration::hours(self.token_expiry_hours);
        if now > expires_at {
            return Err(TrustedDeviceError::TokenExpired { expired_at: expires_at });
        }

        device.trust_status = TrustStatus::Trusted;
        device.verification_token = None;
        device.last_verified_at = Some(now);
        self.save()?;
        Ok(())
    }

    /// Check if the current device is trusted.
    ///
    /// Uses the machine-derived device ID to look up trust status.
    pub fn check_trust(&self) -> Result<bool, TrustedDeviceError> {
        let device_id = Self::generate_device_id();
        match self.devices.get(&device_id) {
            Some(record) => {
                if record.is_trusted() {
                    Ok(true)
                } else {
                    Err(TrustedDeviceError::DeviceNotTrusted { device_id })
                }
            }
            None => Err(TrustedDeviceError::DeviceNotTrusted { device_id }),
        }
    }

    /// Revoke trust for a device.
    pub fn revoke_device(&mut self, device_id: &str) -> Result<(), TrustedDeviceError> {
        if let Some(record) = self.devices.get_mut(device_id) {
            record.trust_status = TrustStatus::Revoked;
            record.verification_token = None;
            record.last_verified_at = Some(Utc::now());
            self.save()?;
            Ok(())
        } else {
            Err(TrustedDeviceError::DeviceNotTrusted {
                device_id: device_id.to_string(),
            })
        }
    }

    /// List all registered devices.
    #[must_use]
    pub fn list_devices(&self) -> Vec<&DeviceRecord> {
        self.devices.values().collect()
    }

    /// Get a device by ID.
    #[must_use]
    pub fn get_device(&self, device_id: &str) -> Option<&DeviceRecord> {
        self.devices.get(device_id)
    }

    /// Generate a deterministic device ID from machine info.
    fn generate_device_id() -> String {
        let input = {
            let mut data = Vec::new();
            if let Ok(hostname) = std::env::var("HOSTNAME") {
                data.extend_from_slice(hostname.as_bytes());
            }
            if let Ok(user) = std::env::var("USER") {
                data.extend_from_slice(user.as_bytes());
            }
            if data.is_empty() {
                data.extend_from_slice(b"coderex-default-device");
            }
            data
        };
        format!("{:x}", Sha256::digest(input))
    }

    /// Generate a verification token for a device.
    fn generate_verification_token(device_id: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(device_id.as_bytes());
        hasher.update(Utc::now().timestamp_nanos_opt().unwrap_or(0).to_be_bytes());
        let result = hasher.finalize();
        // Use first 16 hex chars as a short token
        format!("{result:x}")[..16].to_string()
    }

    /// Check if a given device ID belongs to the specified organization.
    #[must_use]
    pub fn device_belongs_to_org(&self, device_id: &str, org_id: &str) -> bool {
        self.devices
            .get(device_id)
            .is_some_and(|d| d.org_id.as_deref() == Some(org_id))
    }

    /// Count trusted devices for an organization.
    #[must_use]
    pub fn count_trusted_devices(&self, org_id: &str) -> usize {
        self.devices
            .values()
            .filter(|d| d.org_id.as_deref() == Some(org_id) && d.is_trusted())
            .count()
    }
}

/// Compute a machine fingerprint for device identification.
#[must_use]
pub fn machine_fingerprint() -> String {
    let mut hasher = Sha256::new();
    if let Ok(hostname) = std::env::var("HOSTNAME") {
        hasher.update(hostname.as_bytes());
    }
    if let Ok(user) = std::env::var("USER") {
        hasher.update(user.as_bytes());
    }
    if let Ok(home) = std::env::var("HOME") {
        hasher.update(home.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Default store path for trusted devices.
#[must_use]
pub fn default_device_store_path() -> PathBuf {
    let config_dir = crate::config_home();
    config_dir.join("trusted_devices.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry() -> (TrustedDeviceRegistry, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trusted_devices.json");
        let registry = TrustedDeviceRegistry::new(path);
        (registry, tmp)
    }

    #[test]
    fn register_device_returns_token() {
        let (mut reg, _tmp) = temp_registry();
        let token = reg.register_device("my-laptop", None).unwrap();
        assert!(!token.is_empty());
        assert_eq!(reg.list_devices().len(), 1);
    }

    #[test]
    fn register_device_is_pending() {
        let (mut reg, _tmp) = temp_registry();
        reg.register_device("my-laptop", None).unwrap();
        let device = reg.list_devices()[0];
        assert_eq!(device.trust_status, TrustStatus::Pending);
        assert!(!device.is_trusted());
    }

    #[test]
    fn register_device_with_org_id() {
        let (mut reg, _tmp) = temp_registry();
        reg.register_device("work-laptop", Some("acme-corp")).unwrap();
        let device = reg.list_devices()[0];
        assert_eq!(device.org_id.as_deref(), Some("acme-corp"));
    }

    #[test]
    fn register_device_twice_fails() {
        let (mut reg, _tmp) = temp_registry();
        // Same machine gets same device_id, so second registration fails
        reg.register_device("my-laptop", None).unwrap();
        let result = reg.register_device("my-laptop", None);
        assert!(matches!(result, Err(TrustedDeviceError::DeviceAlreadyRegistered { .. })));
    }

    #[test]
    fn verify_device_marks_trusted() {
        let (mut reg, _tmp) = temp_registry();
        let token = reg.register_device("my-laptop", None).unwrap();
        reg.verify_device(&token).unwrap();
        let device = reg.list_devices()[0];
        assert_eq!(device.trust_status, TrustStatus::Trusted);
        assert!(device.is_trusted());
        assert!(device.verification_token.is_none());
    }

    #[test]
    fn verify_device_invalid_token_fails() {
        let (mut reg, _tmp) = temp_registry();
        reg.register_device("my-laptop", None).unwrap();
        let result = reg.verify_device("wrong-token");
        assert!(matches!(result, Err(TrustedDeviceError::InvalidToken)));
    }

    #[test]
    fn check_trust_before_verification_fails() {
        let (mut reg, _tmp) = temp_registry();
        reg.register_device("my-laptop", None).unwrap();
        let result = reg.check_trust();
        assert!(matches!(result, Err(TrustedDeviceError::DeviceNotTrusted { .. })));
    }

    #[test]
    fn check_trust_after_verification_succeeds() {
        let (mut reg, _tmp) = temp_registry();
        let token = reg.register_device("my-laptop", None).unwrap();
        reg.verify_device(&token).unwrap();
        assert!(reg.check_trust().unwrap());
    }

    #[test]
    fn revoke_device() {
        let (mut reg, _tmp) = temp_registry();
        let token = reg.register_device("my-laptop", None).unwrap();
        reg.verify_device(&token).unwrap();
        let device_id = reg.list_devices()[0].device_id.clone();
        reg.revoke_device(&device_id).unwrap();
        assert!(matches!(reg.check_trust(), Err(TrustedDeviceError::DeviceNotTrusted { .. })));
        let device = reg.get_device(&device_id).unwrap();
        assert_eq!(device.trust_status, TrustStatus::Revoked);
    }

    #[test]
    fn device_belongs_to_org() {
        let (mut reg, _tmp) = temp_registry();
        reg.register_device("work", Some("acme")).unwrap();
        // Device ID is deterministic based on env vars
        let device_id = reg.list_devices()[0].device_id.clone();
        assert!(reg.device_belongs_to_org(&device_id, "acme"));
        assert!(!reg.device_belongs_to_org(&device_id, "other"));
    }

    #[test]
    fn count_trusted_devices_per_org() {
        let (mut reg, _tmp) = temp_registry();
        let token1 = reg.register_device("dev1", Some("acme")).unwrap();
        reg.verify_device(&token1).unwrap();

        // Second device would get same ID on same machine, so we test via manual insertion
        let mut device2 = reg.list_devices()[0].clone();
        device2.device_id = "other-device".to_string();
        device2.org_id = Some("other-org".to_string());
        device2.trust_status = TrustStatus::Trusted;
        reg.devices.insert(device2.device_id.clone(), device2);

        assert_eq!(reg.count_trusted_devices("acme"), 1);
        assert_eq!(reg.count_trusted_devices("other-org"), 1);
        assert_eq!(reg.count_trusted_devices("nonexistent"), 0);
    }

    #[test]
    fn device_store_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trusted_devices.json");
        {
            let mut reg = TrustedDeviceRegistry::new(path.clone());
            reg.register_device("my-laptop", None).unwrap();
        }
        {
            let mut reg = TrustedDeviceRegistry::new(path);
            reg.load().unwrap();
            assert_eq!(reg.list_devices().len(), 1);
        }
    }

    #[test]
    fn machine_fingerprint_is_deterministic() {
        let fp1 = machine_fingerprint();
        let fp2 = machine_fingerprint();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn token_expiry_rejects_expired() {
        let (mut reg, _tmp) = temp_registry();
        reg.register_device("my-laptop", None).unwrap();

        // Manually set registered_at to long ago
        let device = reg.list_devices()[0];
        let device_id = device.device_id.clone();
        let token = device.verification_token.clone().unwrap();

        // Simulate expired: set registered_at far in the past
        let mut record = reg.devices.get_mut(&device_id).unwrap().clone();
        record.registered_at = Utc::now() - chrono::Duration::hours(100);
        reg.devices.insert(device_id.clone(), record);

        let result = reg.verify_device(&token);
        assert!(matches!(result, Err(TrustedDeviceError::TokenExpired { .. })));
    }
}
