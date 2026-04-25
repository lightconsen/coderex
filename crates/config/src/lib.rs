//! CodeRex configuration system.

#![warn(missing_docs)]

pub mod dirs;
pub mod experiments;
pub mod flags;
pub mod settings;

#[cfg(feature = "enterprise")]
pub mod org_policy;
#[cfg(feature = "enterprise")]
pub mod remote_settings;
#[cfg(feature = "enterprise")]
pub mod trusted_device;

pub use flags::{CapabilitySet, FeatureFlags, FlagRegistry, percentage_rollout};
#[cfg(feature = "enterprise")]
pub use org_policy::{OrgPolicy, OrgPolicyError, OrgPolicyStore};
#[cfg(feature = "enterprise")]
pub use remote_settings::{
    FetchState, LoadingBarrier, RemoteManagedSettings, RemoteManagedSettingsClient,
    RemoteSettingsError, RemoteSettingsManager, RemoteSettingsResponse,
};
#[cfg(feature = "hot-reload")]
pub use settings::ConfigWatcher;
pub use settings::{
    CURRENT_SCHEMA_VERSION, ConfigDiff, ConfigDiffEntry, ConfigError, ConfigService,
    RuntimeOverrides, Settings, config_home, migrate_settings, resolve_api_key,
};
#[cfg(feature = "enterprise")]
pub use trusted_device::{
    DeviceRecord, TrustStatus, TrustedDeviceError, TrustedDeviceRegistry,
    default_device_store_path, machine_fingerprint,
};
